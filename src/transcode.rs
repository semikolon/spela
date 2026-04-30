use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Apr 30, 2026: scan ffmpeg's stderr log for symptoms of source-file
/// corruption (Hijack S02E05 MeGusta incident class). Returns Some(reason)
/// if any of three signals is present, None if the log is clean.
///
/// Three signals (per Apr 29 incident in CLAUDE.md § "Corrupt source files
/// defeat auto-recast — wedge isn't on the receiver"):
///   1. `invalid as first byte of an EBML number` — Matroska container
///      corruption; ffmpeg's parser hit a byte outside the EBML alphabet.
///   2. `Could not find ref with POC` — HEVC reference-frame missing,
///      decoder can't reconstruct subsequent frames.
///   3. `dup=N` on a final summary line where N > 100 — NVENC silently
///      duplicated >100 frames, almost always because of (1) or (2)
///      upstream corrupting the input pipeline. Threshold tuned at 100
///      to avoid false positives from normal decode hiccups (a few dropped
///      frames at scene changes is fine).
///
/// Used by `do_cleanup` to mark the source path in `AppState.corrupt_files`,
/// so subsequent Local Bypass scans skip the broken file instead of looping
/// the same recast→broken-frames→recast cycle the Apr 29 incident exhibited.
pub fn inspect_ffmpeg_log_for_corruption(log: &str) -> Option<&'static str> {
    if log.contains("invalid as first byte of an EBML number") {
        return Some("Matroska container corruption (EBML parse error)");
    }
    if log.contains("Could not find ref with POC") {
        return Some("HEVC reference frame missing (decoder couldn't reconstruct)");
    }
    // dup=N on a summary line. Walk recent lines (the summary's near the end)
    // and find the first dup= occurrence. Threshold: N > 100.
    for line in log.lines().rev().take(50) {
        if let Some(idx) = line.find("dup=") {
            let after = &line[idx + 4..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u32>() {
                if n > 100 {
                    return Some("excessive frame duplication (NVENC fill from missing refs)");
                }
            }
            return None; // first dup= we hit is the load-bearing one
        }
    }
    None
}

/// Apr 29, 2026: rotate `ffmpeg.log` ring-buffer style before truncating.
///
/// Both `transcode()` and `transcode_hls()` open `~/.spela/ffmpeg.log` with
/// `File::create`, which truncates to zero. That destroyed the previous
/// stream's diagnostic data the moment the next stream started. The Apr 28
/// H.264 5-min freeze was unrecoverable post-mortem because I started a
/// HEVC stream right after, overwriting the log.
///
/// Ring keeps the last 5 logs:
///   `ffmpeg.log` (current write target — about to be truncated)
///   `ffmpeg.log.1` (most recent past)
///   ...
///   `ffmpeg.log.5` (oldest kept; next rotation deletes it)
///
/// Best-effort: any rename/remove failure is logged but doesn't stop the
/// caller — log preservation is a debugging aid, not a correctness path.
pub fn rotate_ffmpeg_log(current: &Path) {
    if !current.exists() {
        return;
    }
    const KEEP: usize = 5;
    // Drop the oldest if it exists, then shift each .N to .N+1.
    let with_n = |n: usize| -> PathBuf {
        let mut p = current.as_os_str().to_os_string();
        p.push(format!(".{n}"));
        PathBuf::from(p)
    };
    let oldest = with_n(KEEP);
    if oldest.exists() {
        let _ = std::fs::remove_file(&oldest);
    }
    for n in (1..KEEP).rev() {
        let src = with_n(n);
        let dst = with_n(n + 1);
        if src.exists() {
            if let Err(e) = std::fs::rename(&src, &dst) {
                tracing::warn!(
                    "rotate_ffmpeg_log: rename {src:?}→{dst:?} failed: {e}"
                );
            }
        }
    }
    let dst = with_n(1);
    if let Err(e) = std::fs::rename(current, &dst) {
        tracing::warn!(
            "rotate_ffmpeg_log: rename {current:?}→{dst:?} failed: {e}"
        );
    }
}

/// Shift all timestamps in an SRT file by `-offset_seconds`, writing the
/// result to a new file. Subtitle entries that end before time 0 are
/// dropped (their entire duration lies before the seek point), and
/// entries that straddle 0 are clamped to start at 0.
///
/// Why this exists: `ffmpeg -ss N -i input -vf subtitles=sub.srt` uses a
/// fast input seek (decodes from offset N), and ffmpeg's default behavior
/// is to RESET output timestamps to start at 0. The `subtitles=` filter
/// reads the SRT file with source-time stamps (entry for source time 0
/// says "show this at time 0") and matches those against the filter's
/// frame timestamps — which, with fast input seek + default timestamp
/// handling, are the RESET output timestamps (0, 1, 2...). So subtitle
/// "for source time 0" overlays on frame "source time N" content → every
/// subtitle appears at the wrong moment, specifically shifted by N
/// seconds.
///
/// Fix: before invoking ffmpeg with a seek offset, physically shift the
/// SRT file so that the entry for "source time N" now says "time 0". The
/// filter matches shifted SRT against reset PTS, timing is correct.
///
/// Alternative approaches considered and rejected:
/// - `-copyts`: preserves source PTS through filter, but then output PTS
///   starts at N (not 0), which breaks HLS / Chromecast expectations.
/// - `-output_ts_offset -N`: attempts to subtract N at the muxer, but
///   interacts poorly with the HLS muxer's segment timestamp math and
///   produces inconsistent segment durations.
/// - Output seek (`-i input -ss N` instead of `-ss N -i input`): correct
///   subtitle timing but 30× slower (decodes + discards everything before N).
/// - `ffmpeg -itsoffset -N -i sub.srt` preprocess pass: works, but
///   introduces a second ffmpeg invocation per play. The Rust shifter
///   below is ~50 lines, no ffmpeg dependency, fully unit-testable.
///
/// Returns the number of subtitle entries written to the output file.
/// Apr 15, 2026 fix for the "subtitles out of sync on resume" regression.
pub fn shift_srt(input: &Path, output: &Path, offset_seconds: f64) -> Result<usize> {
    let content = std::fs::read_to_string(input)?
        .replace("\r\n", "\n");  // Normalize CRLF → LF (OpenSubtitles often uses Windows line endings)
    let mut result = String::new();
    let mut kept = 0usize;
    let mut new_index = 1usize;

    for raw_block in content.split("\n\n") {
        let block = raw_block.trim_end_matches('\n');
        if block.is_empty() {
            continue;
        }
        let lines: Vec<&str> = block.lines().collect();
        // An SRT entry has at least 3 lines: index, timestamps, text.
        // Some SRT files omit the leading blank line (BOM / single-space
        // separators), so we allow the first numeric line to be absent.
        let (ts_line_idx, text_start_idx) = if lines.len() >= 3 && lines[0].parse::<u32>().is_ok() {
            (1, 2)
        } else if lines.len() >= 2 && lines[0].contains(" --> ") {
            (0, 1)
        } else {
            continue;
        };

        let Some((start, end)) = parse_srt_timestamp_line(lines[ts_line_idx]) else {
            continue;
        };
        let new_start = start - offset_seconds;
        let new_end = end - offset_seconds;
        if new_end < 0.0 {
            // Entry ends before the seek point — drop it entirely.
            continue;
        }
        let clamped_start = new_start.max(0.0);
        let new_ts_line = format_srt_timestamp_line(clamped_start, new_end);

        result.push_str(&new_index.to_string());
        result.push('\n');
        result.push_str(&new_ts_line);
        result.push('\n');
        for text_line in &lines[text_start_idx..] {
            result.push_str(text_line);
            result.push('\n');
        }
        result.push('\n');
        new_index += 1;
        kept += 1;
    }

    std::fs::write(output, result)?;
    Ok(kept)
}

fn parse_srt_timestamp_line(line: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = line.split(" --> ").collect();
    if parts.len() != 2 {
        return None;
    }
    Some((
        parse_srt_timestamp(parts[0].trim())?,
        parse_srt_timestamp(parts[1].trim())?,
    ))
}

/// Parse `HH:MM:SS,mmm` → total seconds as f64.
fn parse_srt_timestamp(s: &str) -> Option<f64> {
    // Split on ':' then ',' to get [H, M, S, mmm].
    let first: Vec<&str> = s.splitn(3, ':').collect();
    if first.len() != 3 {
        return None;
    }
    let h: u64 = first[0].parse().ok()?;
    let m: u64 = first[1].parse().ok()?;
    let sec_ms: Vec<&str> = first[2].splitn(2, ',').collect();
    if sec_ms.len() != 2 {
        return None;
    }
    let s: u64 = sec_ms[0].parse().ok()?;
    let ms: u64 = sec_ms[1].parse().ok()?;
    Some(h as f64 * 3600.0 + m as f64 * 60.0 + s as f64 + ms as f64 / 1000.0)
}

fn format_srt_timestamp_line(start: f64, end: f64) -> String {
    format!(
        "{} --> {}",
        format_srt_timestamp(start),
        format_srt_timestamp(end)
    )
}

fn format_srt_timestamp(t: f64) -> String {
    let clamped = t.max(0.0);
    let total_ms = (clamped * 1000.0).round() as u64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{:02}:{:02}:{:02},{:03}", h, m, s, ms)
}

/// Decide which subtitle file ffmpeg's `subtitles=` filter should read,
/// applying the SRT shift when a seek offset is active. Extracted as a
/// pure helper so unit tests can verify the four-way decision tree
/// without running ffmpeg. Apr 15, 2026 regression guard.
///
/// Decision table:
///
///   input subtitles | seek_to       | output
///   ----------------+---------------+-----------------------------------------
///   None            | anything      | None (no subs, nothing to shift)
///   Some(orig)      | None or Some(0)| Some(orig)        (pass-through, no seek)
///   Some(orig)      | Some(N>0)     | Some(shifted) if shift_srt succeeds,
///                   |               | else Some(orig) (graceful degradation —
///                   |               | subtitles will be desynced but playback
///                   |               | still works; a `tracing::warn` logs the
///                   |               | failure so the operator can investigate)
///
/// `hls_dir` is the destination directory for the shifted SRT file —
/// lives alongside the HLS segments so `do_cleanup` sweeps it on play end.
pub fn resolve_subtitle_path_for_seek(
    original: Option<&Path>,
    hls_dir: &Path,
    seek_to: Option<f64>,
) -> Option<PathBuf> {
    let orig = original?;
    // No seek, or zero seek → pass-through unchanged.
    let seek = match seek_to {
        Some(s) if s > 0.0 => s,
        _ => return Some(orig.to_path_buf()),
    };
    // No file on disk → pass-through (shift would fail anyway; let the
    // filter chain handle the missing-file case with its own error).
    if !orig.exists() {
        return Some(orig.to_path_buf());
    }
    let shifted = hls_dir.join("subtitle_shifted.srt");
    match shift_srt(orig, &shifted, seek) {
        Ok(0) => {
            // All subtitle entries are before the seek point — nothing to burn.
            // Return None so the transcode skips subtitle burn-in entirely.
            // Previously returned Some(empty file) → ffmpeg "Unable to open" error.
            tracing::info!(
                "Subtitle sync: shifted {} by {:.0}s, 0 entries retained — skipping subtitles for this segment",
                orig.display(),
                seek,
            );
            None
        }
        Ok(n) => {
            tracing::info!(
                "Subtitle sync: shifted {} by {:.0}s, {} entries retained → {}",
                orig.display(),
                seek,
                n,
                shifted.display()
            );
            Some(shifted)
        }
        Err(e) => {
            tracing::warn!(
                "Subtitle sync: shift_srt failed ({}), falling back to original — subtitles will be desynced by {:.0}s",
                e,
                seek
            );
            Some(orig.to_path_buf())
        }
    }
}

/// Detect audio/video codecs and duration of a media URL/file using ffprobe.
/// Codec detection result with preferred audio stream index.
pub struct CodecInfo {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub duration: Option<f64>,
    /// ffmpeg audio stream specifier for the preferred (English) audio track.
    /// e.g., "0:a:1" for the second audio stream. Falls back to "0:a:0" if
    /// no English track found. Used in -map and filter_complex references.
    pub audio_stream: String,
    /// The audio-only index (0-based among audio streams, for filter refs).
    pub audio_index: usize,
}

/// Detect video/audio codecs, duration, and preferred English audio stream.
pub async fn detect_codecs(url: &str) -> Result<CodecInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_type,codec_name,index",
            "-show_entries", "stream_tags=language",
            "-show_entries", "format=duration",
            url,
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    let mut video_codec = None;
    let mut audio_codec = None;
    let mut duration = None;
    let mut current_type: Option<String> = None;
    let mut current_lang: Option<String> = None;

    // Collect all audio streams with their language tags
    struct AudioStream { audio_index: usize, codec: String, lang: String }
    let mut audio_streams: Vec<AudioStream> = Vec::new();
    let mut audio_count = 0usize;

    for line in combined.lines() {
        if line.starts_with("[STREAM]") {
            current_type = None;
            current_lang = None;
        }
        if let Some(ct) = line.strip_prefix("codec_type=") {
            current_type = Some(ct.trim().to_string());
        }
        if let Some(cn) = line.strip_prefix("codec_name=") {
            match current_type.as_deref() {
                Some("video") if video_codec.is_none() => {
                    video_codec = Some(cn.trim().to_string());
                }
                Some("audio") => {
                    let codec = cn.trim().to_string();
                    if audio_codec.is_none() {
                        audio_codec = Some(codec.clone());
                    }
                    // We'll finalize the AudioStream at [/STREAM]
                    // For now just note the codec
                    current_lang = current_lang.or(Some(String::new()));
                    // Temporarily store codec in current_lang's place
                    // Actually, let's collect at [/STREAM]
                }
                _ => {}
            }
        }
        if let Some(lang) = line.strip_prefix("TAG:language=") {
            current_lang = Some(lang.trim().to_lowercase());
        }
        if line.starts_with("[/STREAM]") {
            if current_type.as_deref() == Some("audio") {
                let codec = audio_codec.clone().unwrap_or_default();
                let lang = current_lang.take().unwrap_or_default();
                audio_streams.push(AudioStream {
                    audio_index: audio_count,
                    codec: codec,
                    lang,
                });
                audio_count += 1;
            }
            current_type = None;
            current_lang = None;
        }
        if let Some(dur) = line.strip_prefix("duration=") {
            if let Ok(d) = dur.trim().parse::<f64>() {
                duration = Some(d);
            }
        }
    }

    // Pick preferred audio: first English track, else first track
    let preferred = audio_streams.iter()
        .find(|a| a.lang == "eng" || a.lang == "en")
        .or_else(|| audio_streams.first());

    let (audio_index, preferred_codec) = match preferred {
        Some(a) => {
            tracing::info!("Preferred audio: stream a:{} lang={} codec={}", a.audio_index, a.lang, a.codec);
            (a.audio_index, Some(a.codec.clone()))
        }
        None => (0, audio_codec.clone()),
    };

    Ok(CodecInfo {
        video_codec,
        audio_codec: preferred_codec.or(audio_codec),
        duration,
        audio_stream: format!("0:a:{}", audio_index),
        audio_index,
    })
}

/// Returns true if the audio codec needs transcoding for Chromecast.
pub fn audio_needs_transcode(codec: &str) -> bool {
    matches!(codec, "ac3" | "eac3" | "dts" | "truehd" | "dca")
}

/// Returns true if the video codec needs transcoding for Chromecast.
/// Basic Chromecasts only support H.264. HEVC/VP9/AV1 need re-encoding.
pub fn video_needs_transcode(codec: &str) -> bool {
    matches!(codec, "hevc" | "h265" | "vp9" | "av1")
}

/// Resolve intro clip path from config directory.
pub fn find_intro() -> Option<PathBuf> {
    let config_dir = dirs::config_dir()?.join("spela").join("intro.mp4");
    if config_dir.exists() {
        Some(config_dir)
    } else {
        None
    }
}

/// Transcode media from an HTTP URL (progressive/streaming input) into a
/// chunked-transfer fragmented MP4 served by `/stream/transcode`.
///
/// **Deprecated for new chromecast plays as of Apr 15, 2026** — the
/// chunked-transfer fMP4 path is rejected by Chromecast Default Media
/// Receiver (see `transcode_hls` doc comment for the full diagnosis). New
/// plays go through `transcode_hls`. This function is kept for the Custom
/// Cast Receiver flow (which uses Shaka Player and a different stream URL
/// path) and as a fallback for non-chromecast targets that can handle
/// chunked-transfer fMP4.
#[allow(dead_code)]
pub async fn transcode(
    input_url: &str,
    media_dir: &Path,
    subtitle_path: Option<&Path>,
    intro_path: Option<&Path>,
    video_reencode: bool,
    seek_to: Option<f64>,
    audio_index: usize,
) -> Result<(PathBuf, u32)> {
    let output_path = media_dir.join("transcoded_aac.mp4");
    let has_intro = intro_path.is_some();
    let has_subs = subtitle_path.map_or(false, |p| p.exists());

    let mut args: Vec<String> = Vec::new();

    // Input 0: intro clip (if present, no -re — plays at full speed)
    if let Some(intro) = intro_path {
        args.extend(["-i".into(), intro.to_string_lossy().to_string()]);
    }

    // Input 1 (or 0 if no intro): main stream
    if let Some(seek) = seek_to {
        if seek > 0.0 {
            args.extend(["-ss".into(), seek.to_string()]);
        }
    }
    // Reconnect flags are HTTP-only — they cause FFmpeg to error on file:// URLs
    let is_local_file = input_url.starts_with("file://") || input_url.starts_with("/");
    if !is_local_file {
        args.extend([
            "-reconnect".into(), "1".into(),
            "-reconnect_at_eof".into(), "1".into(),
            "-reconnect_streamed".into(), "1".into(),
            "-reconnect_delay_max".into(), "30".into(),
            "-rw_timeout".into(), "60000000".into(),
        ]);
    }
    // Strip file:// prefix for FFmpeg (it expects bare paths for local files)
    let ffmpeg_input = if let Some(path) = input_url.strip_prefix("file://") {
        path.to_string()
    } else {
        input_url.to_string()
    };
    args.extend(["-i".into(), ffmpeg_input]);

    // Build filter chain based on what's needed
    let main_idx = if has_intro { 1 } else { 0 };

    if has_intro {
        // Concat requires re-encoding both streams via NVENC.
        // Scale both to 1080p for safe concat (intro is already 1080p,
        // but main stream might vary). Apply subtitles to main if needed.
        let mut filter = String::new();

        // Apr 28, 2026: ALL video paths through h264_nvenc must end with
        // format=yuv420p. NVENC's h264 encoder rejects 10-bit input
        // (yuv420p10le, common for HEVC Main 10 sources like ELiTE/MeGusta
        // releases): "10 bit encode not supported / No capable devices
        // found / Nothing was written into output file" → 0 segments → cast
        // IDLE at <init>. Forcing yuv420p as the last filter step
        // downconverts to 8-bit before NVENC sees the frames. No-op for
        // already-8-bit inputs.

        // Intro: scale + ensure compatible format
        filter.push_str("[0:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v0]; ");
        filter.push_str("[0:a:0]aresample=48000[a0]; ");  // Intro always has one audio track

        // Main stream: scale + optional subtitles
        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            filter.push_str(&format!(
                "[{}:v]subtitles='{}',scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v1]; ",
                main_idx, srt_str
            ));
        } else {
            filter.push_str(&format!(
                "[{}:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v1]; ",
                main_idx
            ));
        }
        filter.push_str(&format!("[{}:a:{}]aresample=48000[a1]; ", main_idx, audio_index));

        // Concat
        filter.push_str("[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]");

        args.extend([
            "-filter_complex".into(), filter,
            "-map".into(), "[v]".into(),
            "-map".into(), "[a]".into(),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
            // Apr 30, 2026 — intro-concat fix (Apr 18 hypothesis, Apr 19 proposal).
            // The intro→main seam is a hard scene change; NVENC's default
            // scene-detection inserts an unscheduled IDR there, desyncing GOP
            // cadence from the HLS segmenter's 6s clock. By segment 5 the
            // accumulated drift causes either (a) a non-keyframe segment start,
            // or (b) EXTINF-vs-actual-duration mismatch beyond Shaka's tolerance
            // → CrKey 1.56 buffers indefinitely. Belt-and-suspenders fix:
            //   -g 180 -keyint_min 180 — fixed 6s GOP at the 30fps the filter pipes
            //   -sc_threshold 0          — disable scene-change IDR injection (x264-flavored,
            //                              NVENC may ignore but harmless)
            //   -force_key_frames expr:gte(t,n_forced*6) — guarantees an IDR at every
            //                              6s boundary regardless of encoder cadence;
            //                              this alone would suffice but the others
            //                              constrain encoder choice for predictability.
            "-g".into(), "180".into(),
            "-keyint_min".into(), "180".into(),
            "-sc_threshold".into(), "0".into(),
            "-force_key_frames".into(), "expr:gte(t,n_forced*6)".into(),
        ]);
    } else {
        // No intro: explicitly map video + preferred audio track.
        // Without -map, ffmpeg picks the DEFAULT audio (could be Russian).
        args.extend([
            "-map".into(), "0:v:0".into(),
            "-map".into(), format!("0:a:{}", audio_index),
        ]);

        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            args.extend([
                // format=yuv420p forces 8-bit before NVENC — see comment above.
                "-vf".into(), format!("subtitles='{}',format=yuv420p", srt_str),
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p4".into(),
                "-cq".into(), "23".into(),
            ]);
        } else if video_reencode {
            args.extend([
                // format=yuv420p forces 8-bit before NVENC — see comment above.
                "-vf".into(), "format=yuv420p".into(),
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p4".into(),
                "-cq".into(), "23".into(),
            ]);
        } else {
            args.extend(["-c:v".into(), "copy".into()]);
        }
    }

    args.extend([
        "-c:a".into(), "aac".into(),
        "-ac".into(), "2".into(),
        "-b:a".into(), "192k".into(),
        "-dn".into(),                // Strip data streams (bin_data) — Chromecast rejects them
        "-map_metadata".into(), "-1".into(), // Strip metadata tracks
        "-movflags".into(), "frag_keyframe+empty_moov+default_base_moof".into(),
        "-y".into(),
        output_path.to_str().unwrap_or("transcoded_aac.mp4").into(),
    ]);

    tracing::info!("ffmpeg args: {:?}", args);

    // Log FFmpeg stderr to a file for debugging (was /dev/null — invisible failures).
    // Apr 29, 2026: rotate before truncating so the LAST stream's log
    // survives the next stream's File::create. Without this, debugging an
    // intermittent failure that recurs the next play is impossible — the
    // first stream's log gets overwritten when the second stream starts.
    // (Apr 28 incident: lost the H.264 5-min freeze evidence by switching
    // to HEVC mid-investigation.)
    let ffmpeg_log_path = media_dir.parent()
        .and_then(|_| dirs::home_dir())
        .map(|h| h.join(".spela").join("ffmpeg.log"))
        .unwrap_or_else(|| media_dir.join("ffmpeg.log"));
    rotate_ffmpeg_log(&ffmpeg_log_path);
    let stderr_file = std::fs::File::create(&ffmpeg_log_path)
        .unwrap_or_else(|_| std::fs::File::create("/tmp/spela-ffmpeg.log").expect("Cannot create any log file"));
    tracing::info!("FFmpeg stderr → {:?}", ffmpeg_log_path);

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()?;

    let pid = child.id().unwrap_or(0);

    // Spawn a background task to reap the child when it exits.
    // Without this, killed ffmpeg processes become zombies because
    // nobody calls waitpid() on them. The task just awaits completion
    // and discards the result — we track liveness by PID elsewhere.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok((output_path, pid))
}

/// Transcode media to HLS (HTTP Live Streaming) instead of fragmented MP4.
///
/// **Why this exists** (Apr 15, 2026): Chromecast Default Media Receiver
/// rejects spela's chunked-transfer fMP4 endpoint — the player engine loads
/// the LOAD message, the receiver acknowledges it, but `player_state` stays
/// IDLE and the TV shows the blue cast icon ("blue cast icon" failure mode).
/// Confirmed against [Igalia/cog#463](https://github.com/Igalia/cog/issues/463):
/// "media framework requires either a known content length or complete data
/// delivery, not incremental chunked delivery." HLS is the format the
/// receiver is built around (Shaka Player handles it natively), and the same
/// content served as an HLS manifest + segments plays without any of the
/// fMP4 chunked-transfer issues.
///
/// Output layout under `<media_dir>/transcoded_hls/`:
///   - `playlist.m3u8` — the HLS manifest (event-type, appendable, finalized
///     with ENDLIST when ffmpeg closes)
///   - `init.mp4`      — the fmp4 init segment (moov box)
///   - `seg_00001.m4s`, `seg_00002.m4s`, ... — 6-second fmp4 segments
///
/// Returns `(playlist_path, ffmpeg_pid)`. Caller is expected to:
///   1. Wait for `playlist.m3u8` + `init.mp4` + at least one `seg_*.m4s` to
///      exist before sending the cast LOAD message.
///   2. Cast the URL `http://<stream_host>:<port>/hls/playlist.m3u8` with
///      content-type `application/vnd.apple.mpegurl`.
///   3. On stop, kill the ffmpeg PID and delete `<media_dir>/transcoded_hls/`
///      recursively (handled by `do_cleanup` in server.rs).
///
/// Mirrors the filter chain of `transcode()` for intro concat + subtitle
/// burn-in + NVENC video re-encode + AAC audio. The only difference from
/// `transcode()` is the muxer: `-f hls -hls_segment_type fmp4 ...` instead
/// of `-movflags frag_keyframe+empty_moov+default_base_moof`.
pub async fn transcode_hls(
    input_url: &str,
    media_dir: &Path,
    subtitle_path: Option<&Path>,
    intro_path: Option<&Path>,
    video_reencode: bool,
    seek_to: Option<f64>,
    audio_index: usize,
) -> Result<(PathBuf, u32)> {
    let hls_dir = media_dir.join("transcoded_hls");

    // Fresh play, fresh segments. Old segments + manifest get wiped here
    // rather than waiting for `do_cleanup` so we never serve stale content
    // from a prior play that didn't reach `do_cleanup`.
    let _ = std::fs::remove_dir_all(&hls_dir);
    std::fs::create_dir_all(&hls_dir)?;

    // Subtitle sync fix (Apr 15, 2026): if we're seeking AND we have a
    // subtitle file, physically shift the SRT so its "time 0" lines up
    // with the frame content at the seek offset. See shift_srt() for
    // rationale. Extracted into resolve_subtitle_path_for_seek() so
    // unit tests can validate the shift-or-passthrough decision without
    // launching ffmpeg.
    let effective_subtitle_path = resolve_subtitle_path_for_seek(
        subtitle_path,
        &hls_dir,
        seek_to,
    );
    let subtitle_path = effective_subtitle_path.as_deref();

    let manifest_path = hls_dir.join("playlist.m3u8");
    // MPEG-TS segments instead of fmp4 (Apr 15, 2026): Default Media Receiver
    // (CAF) requires `media.hlsSegmentFormat = "fmp4"` in the LOAD message
    // for fmp4-HLS to play, but rust_cast's Media struct doesn't expose
    // hlsSegmentFormat — only contentId / contentType / streamType /
    // duration / metadata. Without that signaling, the receiver doesn't
    // know how to parse the .m4s segments and silently stays IDLE. MPEG-TS
    // segments are auto-detected from the manifest's
    // `#EXT-X-PLAYLIST-TYPE` line and don't require any extra LOAD-message
    // fields. ~3% more bandwidth overhead vs fmp4, fully supported.
    let segment_pattern = hls_dir.join("seg_%05d.ts");

    let has_intro = intro_path.is_some();
    let has_subs = subtitle_path.map_or(false, |p| p.exists());

    let mut args: Vec<String> = Vec::new();

    // Input 0: intro clip (if present)
    if let Some(intro) = intro_path {
        args.extend(["-i".into(), intro.to_string_lossy().to_string()]);
    }

    // Seek BEFORE the main input. Cheap seek for keyframe-aligned positions.
    if let Some(seek) = seek_to {
        if seek > 0.0 {
            args.extend(["-ss".into(), seek.to_string()]);
        }
    }

    // Reconnect flags are HTTP-only — they cause FFmpeg to error on file:// URLs
    let is_local_file = input_url.starts_with("file://") || input_url.starts_with("/");
    if !is_local_file {
        args.extend([
            "-reconnect".into(), "1".into(),
            "-reconnect_at_eof".into(), "1".into(),
            "-reconnect_streamed".into(), "1".into(),
            "-reconnect_delay_max".into(), "30".into(),
            "-rw_timeout".into(), "60000000".into(),
        ]);
    }
    let ffmpeg_input = if let Some(path) = input_url.strip_prefix("file://") {
        path.to_string()
    } else {
        input_url.to_string()
    };
    args.extend(["-i".into(), ffmpeg_input]);

    // Filter chain mirrors `transcode()`: intro concat / subs burn-in / video
    // re-encode all happen the same way, only the muxer differs.
    let main_idx = if has_intro { 1 } else { 0 };

    if has_intro {
        let mut filter = String::new();
        filter.push_str("[0:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v0]; ");
        filter.push_str("[0:a:0]aresample=48000[a0]; ");  // Intro always has one audio track
        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            filter.push_str(&format!(
                "[{}:v]subtitles='{}',scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v1]; ",
                main_idx, srt_str
            ));
        } else {
            filter.push_str(&format!(
                "[{}:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v1]; ",
                main_idx
            ));
        }
        filter.push_str(&format!("[{}:a:{}]aresample=48000[a1]; ", main_idx, audio_index));
        filter.push_str("[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]");
        args.extend([
            "-filter_complex".into(), filter,
            "-map".into(), "[v]".into(),
            "-map".into(), "[a]".into(),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
            // Apr 30, 2026 intro-concat fix — see transcode() above for the
            // full rationale. Same four args, same hypothesis.
            "-g".into(), "180".into(),
            "-keyint_min".into(), "180".into(),
            "-sc_threshold".into(), "0".into(),
            "-force_key_frames".into(), "expr:gte(t,n_forced*6)".into(),
        ]);
    } else {
        // No intro: explicitly map video + preferred audio track.
        args.extend([
            "-map".into(), "0:v:0".into(),
            "-map".into(), format!("0:a:{}", audio_index),
        ]);

        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            args.extend([
                // format=yuv420p forces 8-bit before NVENC h264 — see Apr 28
                // 10-bit-HEVC fix in transcode() above.
                "-vf".into(), format!("subtitles='{}',format=yuv420p", srt_str),
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p4".into(),
                "-cq".into(), "23".into(),
            ]);
        } else if video_reencode {
            args.extend([
                // format=yuv420p forces 8-bit before NVENC h264 — see Apr 28
                // 10-bit-HEVC fix in transcode() above.
                "-vf".into(), "format=yuv420p".into(),
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p4".into(),
                "-cq".into(), "23".into(),
            ]);
        } else {
            args.extend(["-c:v".into(), "copy".into()]);
        }
    }

    args.extend([
        "-c:a".into(), "aac".into(),
        "-ac".into(), "2".into(),
        "-b:a".into(), "192k".into(),
        "-dn".into(),
        "-map_metadata".into(), "-1".into(),
    ]);

    // HLS muxer — targeted at OLDER Chromecast Default Media Receivers
    // (CrKey 1.56 firmware on 1st-gen sticks), which only support HLS v3
    // manifests reliably. EVENT playlist type and independent_segments
    // both bump the manifest version to v6, which the older Shaka Player
    // can't parse — the device fetches the manifest 4 times in a row then
    // bails to player_state=IDLE / idle_reason=ERROR without ever
    // requesting any segment. Confirmed live on Apr 15, 2026 against
    // Fredriks TV via direct pychromecast LOAD.
    args.extend([
        "-f".into(), "hls".into(),
        // No -hls_version flag: ffmpeg's HLS muxer doesn't accept one. The
        // manifest version is auto-determined by which features get used.
        // Avoiding `-hls_playlist_type event` and `-hls_flags
        // independent_segments` (both HLS v6) keeps the output at v3-v4,
        // which CrKey 1.56 firmware can parse.
        // 6-second segments — Apple's recommended target_duration for HLS,
        // small enough that pre-buffer is fast (~12 seconds of encoded
        // output gives 2 segments, enough for Chromecast to start
        // reliably) and large enough to avoid HTTP-request overhead per
        // second of content.
        "-hls_time".into(), "6".into(),
        // Keep ALL segments in the manifest — no rotation. Spela serves a
        // finite movie, not a 24/7 broadcast, so the full manifest is fine.
        "-hls_list_size".into(), "0".into(),
        // No playlist_type. EVENT bumps to HLS v6 and confuses old
        // receivers; VOD requires waiting for ffmpeg to finish before any
        // playback. The default (no playlist_type tag) gives live
        // behavior on the manifest while ffmpeg writes segments and is
        // automatically marked complete with #EXT-X-ENDLIST when ffmpeg
        // exits — exactly what we want.
        // MPEG-TS segments (NOT fmp4) so that Default Media Receiver / CAF
        // can play them without requiring `media.hlsSegmentFormat = "fmp4"`
        // in the LOAD message — a field that rust_cast's high-level Media
        // struct doesn't expose.
        "-hls_segment_type".into(), "mpegts".into(),
        "-hls_segment_filename".into(), segment_pattern.to_string_lossy().to_string(),
        // temp_file ONLY: write each segment to .tmp first, then atomically
        // rename. Avoids a partial-segment race with the HTTP serve path.
        // No `independent_segments` — that flag bumps the manifest version
        // and old Shaka Player on CrKey 1.56 can't handle the resulting
        // EXT-X-INDEPENDENT-SEGMENTS tag.
        "-hls_flags".into(), "temp_file".into(),
        "-y".into(),
        manifest_path.to_string_lossy().to_string(),
    ]);

    tracing::info!("ffmpeg HLS args: {:?}", args);

    // Reuse the same stderr log path as the fMP4 transcode for unified debug.
    let ffmpeg_log_path = media_dir.parent()
        .and_then(|_| dirs::home_dir())
        .map(|h| h.join(".spela").join("ffmpeg.log"))
        .unwrap_or_else(|| media_dir.join("ffmpeg.log"));
    let stderr_file = std::fs::File::create(&ffmpeg_log_path)
        .unwrap_or_else(|_| std::fs::File::create("/tmp/spela-ffmpeg.log").expect("Cannot create any log file"));
    tracing::info!("FFmpeg HLS stderr → {:?}", ffmpeg_log_path);

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()?;

    let pid = child.id().unwrap_or(0);

    // Reap on exit — same zombie-prevention pattern as `transcode()`.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok((manifest_path, pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Apr 30, 2026: corrupt-source-file detection (Hijack S02E05 incident) ---

    #[test]
    fn inspect_ffmpeg_log_detects_ebml_corruption() {
        let log = "frame= 100 fps=23 size= 500kB time=00:00:04 bitrate= 983kbits/s speed=0.95x\n\
            [matroska,webm @ 0x55a8b8b3e780] 0x00 at pos 417448081 (0x18e1c091) invalid as first byte of an EBML number\n\
            frame= 500 fps=24 size= 2000kB time=00:00:20 bitrate= 786kbits/s speed=1.0x\n";
        assert_eq!(
            inspect_ffmpeg_log_for_corruption(log),
            Some("Matroska container corruption (EBML parse error)")
        );
    }

    #[test]
    fn inspect_ffmpeg_log_detects_missing_hevc_ref() {
        let log = "[hevc @ 0x7f8c40000000] Could not find ref with POC 54458, 54456, 54452\n";
        assert_eq!(
            inspect_ffmpeg_log_for_corruption(log),
            Some("HEVC reference frame missing (decoder couldn't reconstruct)")
        );
    }

    #[test]
    fn inspect_ffmpeg_log_detects_excessive_duplication() {
        let log = "frame= 9000 fps=24 q=23.0 size= 450000kB time=00:06:15.00 \
            bitrate=9805.4kbits/s dup=1500 drop=0 speed=1.5x\n";
        assert_eq!(
            inspect_ffmpeg_log_for_corruption(log),
            Some("excessive frame duplication (NVENC fill from missing refs)")
        );
    }

    #[test]
    fn inspect_ffmpeg_log_clean_returns_none() {
        let log = "frame= 9000 fps=24 q=23.0 size= 450000kB time=00:06:15 \
            bitrate=9805kbits/s dup=5 drop=0 speed=1.5x\n";
        assert_eq!(inspect_ffmpeg_log_for_corruption(log), None);
    }

    #[test]
    fn inspect_ffmpeg_log_dup_threshold_is_exclusive_at_100() {
        // Threshold is N > 100. dup=100 should NOT trigger; dup=101 should.
        // Defensive against false positives from normal short scenes.
        let at = "frame= 1000 fps=24 dup=100 drop=0 speed=1x\n";
        let just_above = "frame= 1000 fps=24 dup=101 drop=0 speed=1x\n";
        assert_eq!(inspect_ffmpeg_log_for_corruption(at), None);
        assert!(inspect_ffmpeg_log_for_corruption(just_above).is_some());
    }

    #[test]
    fn inspect_ffmpeg_log_empty_input_returns_none() {
        // Defensive: empty log (e.g. transcode never started) shouldn't
        // panic or false-positive.
        assert_eq!(inspect_ffmpeg_log_for_corruption(""), None);
    }

    // Apr 29, 2026: rotate_ffmpeg_log preserves debug evidence across stream
    // restarts. Apr 28 incident: lost the H.264 5-min-freeze ffmpeg.log when
    // I started a HEVC stream right after — the log had been overwritten by
    // ffmpeg's File::create. These tests pin that the ring rotates correctly
    // and that nothing crashes when files are missing.

    #[test]
    fn test_rotate_creates_dot1_from_existing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ffmpeg.log");
        std::fs::write(&log, "stream A").unwrap();
        rotate_ffmpeg_log(&log);
        assert!(!log.exists(), "current was rotated away");
        assert_eq!(std::fs::read_to_string(log.with_extension("log.1")).unwrap(), "stream A");
    }

    #[test]
    fn test_rotate_shifts_existing_history() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ffmpeg.log");
        std::fs::write(&log, "current").unwrap();
        std::fs::write(log.with_extension("log.1"), "older").unwrap();
        std::fs::write(log.with_extension("log.2"), "older2").unwrap();
        rotate_ffmpeg_log(&log);
        assert_eq!(std::fs::read_to_string(log.with_extension("log.1")).unwrap(), "current");
        assert_eq!(std::fs::read_to_string(log.with_extension("log.2")).unwrap(), "older");
        assert_eq!(std::fs::read_to_string(log.with_extension("log.3")).unwrap(), "older2");
    }

    #[test]
    fn test_rotate_drops_oldest_when_at_keep_limit() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ffmpeg.log");
        std::fs::write(&log, "current").unwrap();
        for n in 1..=5 {
            std::fs::write(log.with_extension(format!("log.{n}")), format!("gen{n}")).unwrap();
        }
        rotate_ffmpeg_log(&log);
        // gen5 (the oldest) should be dropped; gen1..gen4 shifted to .2..=.5
        assert_eq!(std::fs::read_to_string(log.with_extension("log.1")).unwrap(), "current");
        assert_eq!(std::fs::read_to_string(log.with_extension("log.2")).unwrap(), "gen1");
        assert_eq!(std::fs::read_to_string(log.with_extension("log.5")).unwrap(), "gen4");
        // .6 must NOT exist — ring is bounded
        assert!(!log.with_extension("log.6").exists());
    }

    #[test]
    fn test_rotate_no_crash_when_log_missing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ffmpeg.log");
        // No file exists — rotate should be a no-op, not an error.
        rotate_ffmpeg_log(&log);
        assert!(!log.exists());
        assert!(!log.with_extension("log.1").exists());
    }

    #[test]
    fn test_audio_needs_transcode() {
        assert!(audio_needs_transcode("ac3"));
        assert!(audio_needs_transcode("eac3"));
        assert!(audio_needs_transcode("dts"));
        assert!(audio_needs_transcode("truehd"));
        assert!(audio_needs_transcode("dca"));
        assert!(!audio_needs_transcode("aac"));
        assert!(!audio_needs_transcode("mp3"));
        assert!(!audio_needs_transcode("opus"));
        assert!(!audio_needs_transcode("vorbis"));
    }

    #[test]
    fn test_video_needs_transcode() {
        assert!(video_needs_transcode("hevc"));
        assert!(video_needs_transcode("h265"));
        assert!(video_needs_transcode("vp9"));
        assert!(video_needs_transcode("av1"));
        assert!(!video_needs_transcode("h264"));
        assert!(!video_needs_transcode("mpeg4"));
        assert!(!video_needs_transcode("vp8"));
    }

    #[test]
    fn test_find_intro_returns_none_when_missing() {
        // In test env, ~/.config/spela/intro.mp4 likely doesn't exist
        // This just verifies the function doesn't panic
        let _ = find_intro();
    }

    // ===== SRT shifter (Apr 15, 2026 subtitle-sync fix) =====

    #[test]
    fn test_parse_srt_timestamp() {
        assert_eq!(parse_srt_timestamp("00:00:00,000"), Some(0.0));
        assert_eq!(parse_srt_timestamp("00:00:01,500"), Some(1.5));
        assert_eq!(parse_srt_timestamp("00:30:00,000"), Some(1800.0));
        assert_eq!(parse_srt_timestamp("01:03:43,612"), Some(3823.612));
        assert_eq!(parse_srt_timestamp("00:00:12,345"), Some(12.345));
    }

    #[test]
    fn test_parse_srt_timestamp_rejects_malformed() {
        assert_eq!(parse_srt_timestamp(""), None);
        assert_eq!(parse_srt_timestamp("00:00"), None);
        assert_eq!(parse_srt_timestamp("00:00:00"), None); // Missing ,mmm
        assert_eq!(parse_srt_timestamp("xx:yy:zz,aaa"), None);
    }

    #[test]
    fn test_format_srt_timestamp_roundtrip() {
        for secs in [0.0_f64, 1.5, 12.345, 1800.0, 3823.612, 7200.999] {
            let formatted = format_srt_timestamp(secs);
            let reparsed = parse_srt_timestamp(&formatted).unwrap();
            assert!(
                (reparsed - secs).abs() < 0.001,
                "roundtrip lost precision: {} → {} → {}",
                secs,
                formatted,
                reparsed
            );
        }
    }

    #[test]
    fn test_format_srt_timestamp_clamps_negative() {
        // Negative input clamps to 0.0 (protects the output-file format)
        assert_eq!(format_srt_timestamp(-5.0), "00:00:00,000");
    }

    #[test]
    fn test_shift_srt_normalizes_crlf_line_endings() {
        // Apr 18, 2026 incident: OpenSubtitles SRT files use Windows-style
        // \r\n separators, but `split("\n\n")` doesn't match `\r\n\r\n`.
        // Without the .replace("\r\n", "\n") at line 91, the entire file
        // collapses into one block and parsing returns 0 entries → ffmpeg
        // crashes on empty subtitle file → cast fails with blue-icon.
        // Pin the normalization with a CRLF-only fixture.
        let tmp_dir = tempfile::tempdir().unwrap();
        let input_path = tmp_dir.path().join("in.srt");
        let output_path = tmp_dir.path().join("out.srt");
        // CRLF line endings throughout — what OpenSubtitles ships.
        std::fs::write(
            &input_path,
            "1\r\n00:00:05,000 --> 00:00:07,000\r\nHello\r\n\r\n2\r\n00:00:15,000 --> 00:00:18,000\r\nWorld\r\n\r\n",
        )
        .unwrap();

        let kept = shift_srt(&input_path, &output_path, 0.0).unwrap();
        assert_eq!(
            kept, 2,
            "CRLF SRT must parse to 2 entries; pre-Apr-18-fix returned 0"
        );

        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    #[test]
    fn test_shift_srt_simple_forward_shift() {
        // Write a tiny SRT, shift by 10s, check output.
        let tmp_dir = std::env::temp_dir().join("spela_srt_test_1");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let input_path = tmp_dir.join("in.srt");
        let output_path = tmp_dir.join("out.srt");
        std::fs::write(
            &input_path,
            "1\n00:00:05,000 --> 00:00:07,000\nHello\n\n2\n00:00:15,500 --> 00:00:18,000\nWorld\n\n",
        )
        .unwrap();

        // Shift by 10s: entry 1 (5-7s) should be dropped (ends before 0),
        // entry 2 (15.5-18s) should be rewritten to (5.5-8s).
        let kept = shift_srt(&input_path, &output_path, 10.0).unwrap();
        assert_eq!(kept, 1, "Only the entry ending after 10s should remain");

        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(
            result.contains("00:00:05,500 --> 00:00:08,000"),
            "Expected shifted timestamp in output, got:\n{result}"
        );
        assert!(result.contains("World"));
        assert!(!result.contains("Hello"));
        // Re-numbered to 1 (was entry 2 in source)
        assert!(result.starts_with("1\n"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_shift_srt_clamps_straddling_entry() {
        // An entry that starts before 0 but ends after 0 should be kept
        // with its start clamped to 0, not dropped.
        let tmp_dir = std::env::temp_dir().join("spela_srt_test_2");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let input_path = tmp_dir.join("in.srt");
        let output_path = tmp_dir.join("out.srt");
        // Entry says 8-12s, shift by 10s → would be (-2, +2). Start clamped to 0.
        std::fs::write(
            &input_path,
            "1\n00:00:08,000 --> 00:00:12,000\nStraddle\n\n",
        )
        .unwrap();

        let kept = shift_srt(&input_path, &output_path, 10.0).unwrap();
        assert_eq!(kept, 1);

        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(
            result.contains("00:00:00,000 --> 00:00:02,000"),
            "Straddling entry should clamp start to 0, got:\n{result}"
        );
        assert!(result.contains("Straddle"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_shift_srt_zero_offset_is_identity_like() {
        // Shifting by 0 should keep all entries with identical timestamps
        // (apart from re-numbering, which starts at 1 anyway).
        let tmp_dir = std::env::temp_dir().join("spela_srt_test_3");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let input_path = tmp_dir.join("in.srt");
        let output_path = tmp_dir.join("out.srt");
        std::fs::write(
            &input_path,
            "1\n00:00:05,000 --> 00:00:07,000\nA\n\n2\n00:00:15,500 --> 00:00:18,000\nB\n\n",
        )
        .unwrap();

        let kept = shift_srt(&input_path, &output_path, 0.0).unwrap();
        assert_eq!(kept, 2);

        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(result.contains("00:00:05,000 --> 00:00:07,000"));
        assert!(result.contains("00:00:15,500 --> 00:00:18,000"));
        assert!(result.contains("A"));
        assert!(result.contains("B"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_shift_srt_multi_line_text_preserved() {
        // Multi-line subtitle text (line 2 and 3 of the entry) must survive
        // the shift without being merged or dropped.
        let tmp_dir = std::env::temp_dir().join("spela_srt_test_4");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let input_path = tmp_dir.join("in.srt");
        let output_path = tmp_dir.join("out.srt");
        std::fs::write(
            &input_path,
            "1\n00:00:30,000 --> 00:00:33,000\n- First line\n- Second line\n\n",
        )
        .unwrap();

        shift_srt(&input_path, &output_path, 10.0).unwrap();
        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(result.contains("- First line"));
        assert!(result.contains("- Second line"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ===== resolve_subtitle_path_for_seek (the decision tree that wraps shift_srt) =====
    //
    // These tests are THE regression guard against the Apr 15, 2026 subtitle
    // sync bug class. If someone accidentally bypasses the shift in a future
    // transcode_hls refactor (or removes the extracted helper entirely),
    // these tests fail loudly.

    fn resolve_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spela_resolve_test_{}", label));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_resolve_subtitle_none_returns_none() {
        let hls_dir = resolve_temp_dir("none_subs");
        assert!(resolve_subtitle_path_for_seek(None, &hls_dir, None).is_none());
        assert!(resolve_subtitle_path_for_seek(None, &hls_dir, Some(0.0)).is_none());
        assert!(resolve_subtitle_path_for_seek(None, &hls_dir, Some(1800.0)).is_none());
        let _ = std::fs::remove_dir_all(&hls_dir);
    }

    #[test]
    fn test_resolve_subtitle_no_seek_passthrough() {
        // seek_to = None → pass the original path through unchanged.
        let hls_dir = resolve_temp_dir("no_seek");
        let orig = hls_dir.join("orig.srt");
        std::fs::write(&orig, "1\n00:00:05,000 --> 00:00:07,000\nHello\n\n").unwrap();

        let resolved = resolve_subtitle_path_for_seek(Some(&orig), &hls_dir, None).unwrap();
        assert_eq!(resolved, orig);
        assert!(!hls_dir.join("subtitle_shifted.srt").exists());

        let _ = std::fs::remove_dir_all(&hls_dir);
    }

    #[test]
    fn test_resolve_subtitle_zero_seek_passthrough() {
        // seek_to = Some(0.0) is treated as no-seek (cheap optimization).
        // `ss_offset = 0.0` is the default for a non-resume play, and
        // shifting by 0 would be a pointless file-IO round trip.
        let hls_dir = resolve_temp_dir("zero_seek");
        let orig = hls_dir.join("orig.srt");
        std::fs::write(&orig, "1\n00:00:05,000 --> 00:00:07,000\nX\n\n").unwrap();

        let resolved = resolve_subtitle_path_for_seek(Some(&orig), &hls_dir, Some(0.0)).unwrap();
        assert_eq!(resolved, orig);
        assert!(!hls_dir.join("subtitle_shifted.srt").exists());

        let _ = std::fs::remove_dir_all(&hls_dir);
    }

    #[test]
    fn test_resolve_subtitle_positive_seek_uses_shifted() {
        // seek_to = Some(1800) AND subs exist → resolved path MUST be the
        // shifted file, NOT the original. This is the load-bearing assertion
        // that would have caught the Apr 15 subtitle-sync bug.
        let hls_dir = resolve_temp_dir("positive_seek");
        let orig = hls_dir
            .parent()
            .unwrap()
            .join("spela_resolve_positive_seek_orig.srt");
        std::fs::write(
            &orig,
            concat!(
                "1\n00:00:10,000 --> 00:00:12,000\nEarly\n\n",
                "2\n00:30:05,000 --> 00:30:08,000\nAt resume\n\n",
            ),
        )
        .unwrap();

        let resolved =
            resolve_subtitle_path_for_seek(Some(&orig), &hls_dir, Some(1800.0)).unwrap();
        // CRITICAL: the resolved path must NOT be the original.
        assert_ne!(
            resolved, orig,
            "Regression: resolve_subtitle_path_for_seek returned the ORIGINAL path \
             for seek_to=1800. ffmpeg will desync subtitles by 30 min. \
             Fix: ensure shift_srt is called and its output path is returned."
        );
        assert_eq!(resolved, hls_dir.join("subtitle_shifted.srt"));
        assert!(resolved.exists());
        let shifted_content = std::fs::read_to_string(&resolved).unwrap();
        assert!(
            !shifted_content.contains("Early"),
            "Entries before the seek point must be dropped from shifted SRT"
        );
        assert!(
            shifted_content.contains("At resume"),
            "Entries at/after seek point must survive the shift"
        );
        assert!(
            shifted_content.contains("00:00:05,000"),
            "Shifted entry should have timestamp reduced by the seek offset"
        );

        let _ = std::fs::remove_dir_all(&hls_dir);
        let _ = std::fs::remove_file(&orig);
    }

    #[test]
    fn test_resolve_subtitle_missing_file_passthrough() {
        // Edge case: caller passed a path that doesn't exist. Don't crash —
        // pass through, let the ffmpeg filter chain surface the error.
        let hls_dir = resolve_temp_dir("missing");
        let nonexistent = hls_dir.join("does_not_exist.srt");

        let resolved =
            resolve_subtitle_path_for_seek(Some(&nonexistent), &hls_dir, Some(1800.0)).unwrap();
        assert_eq!(resolved, nonexistent);
        assert!(!hls_dir.join("subtitle_shifted.srt").exists());

        let _ = std::fs::remove_dir_all(&hls_dir);
    }

    #[test]
    fn test_shift_srt_realistic_30_minute_seek() {
        // Scenario: user resumes at 1800s. All subtitles before that should
        // vanish; all subtitles at or after should be shifted back by 1800s.
        let tmp_dir = std::env::temp_dir().join("spela_srt_test_5");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let input_path = tmp_dir.join("in.srt");
        let output_path = tmp_dir.join("out.srt");
        std::fs::write(
            &input_path,
            concat!(
                "1\n00:00:10,000 --> 00:00:12,000\nOpening credits\n\n",
                "2\n00:15:00,000 --> 00:15:03,000\nMidway dialogue\n\n",
                "3\n00:30:05,000 --> 00:30:08,000\nAt resume point\n\n",
                "4\n00:45:00,000 --> 00:45:04,000\nNear the end\n\n",
            ),
        )
        .unwrap();

        let kept = shift_srt(&input_path, &output_path, 1800.0).unwrap();
        assert_eq!(kept, 2, "Only entries 3 and 4 should survive a 1800s shift");

        let result = std::fs::read_to_string(&output_path).unwrap();
        assert!(!result.contains("Opening credits"));
        assert!(!result.contains("Midway dialogue"));
        assert!(result.contains("At resume point"));
        assert!(result.contains("Near the end"));

        // Entry 3 at source 30:05 → output 00:05 (5s after shift)
        assert!(result.contains("00:00:05,000 --> 00:00:08,000"));
        // Entry 4 at source 45:00 → output 15:00
        assert!(result.contains("00:15:00,000 --> 00:15:04,000"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
