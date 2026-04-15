use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Detect audio/video codecs and duration of a media URL/file using ffprobe.
/// Returns (video_codec, audio_codec, duration_secs).
pub async fn detect_codecs(url: &str) -> Result<(Option<String>, Option<String>, Option<f64>)> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-show_entries", "format=duration", url])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    let mut video_codec = None;
    let mut audio_codec = None;
    let mut duration = None;
    let mut current_type = None;

    for line in combined.lines() {
        if let Some(ct) = line.strip_prefix("codec_type=") {
            current_type = Some(ct.trim().to_string());
        }
        if let Some(cn) = line.strip_prefix("codec_name=") {
            match current_type.as_deref() {
                Some("video") if video_codec.is_none() => video_codec = Some(cn.trim().to_string()),
                Some("audio") if audio_codec.is_none() => audio_codec = Some(cn.trim().to_string()),
                _ => {}
            }
        }
        if let Some(dur) = line.strip_prefix("duration=") {
            if let Ok(d) = dur.trim().parse::<f64>() {
                duration = Some(d);
            }
        }
    }

    Ok((video_codec, audio_codec, duration))
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

        // Intro: scale + ensure compatible format
        filter.push_str("[0:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v0]; ");
        filter.push_str("[0:a]aresample=48000[a0]; ");

        // Main stream: scale + optional subtitles
        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            filter.push_str(&format!(
                "[{}:v]subtitles='{}',scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v1]; ",
                main_idx, srt_str
            ));
        } else {
            filter.push_str(&format!(
                "[{}:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v1]; ",
                main_idx
            ));
        }
        filter.push_str(&format!("[{}:a]aresample=48000[a1]; ", main_idx));

        // Concat
        filter.push_str("[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]");

        args.extend([
            "-filter_complex".into(), filter,
            "-map".into(), "[v]".into(),
            "-map".into(), "[a]".into(),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else if has_subs {
        // No intro, but subtitles — NVENC re-encode
        let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
            .replace(':', "\\:");
        args.extend([
            "-vf".into(), format!("subtitles='{}'", srt_str),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else if video_reencode {
        // No intro, no subs, but video needs re-encoding (HEVC→H.264)
        args.extend([
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else {
        // No intro, no subs, compatible video — video copy (cheapest path)
        args.extend(["-c:v".into(), "copy".into()]);
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

    // Log FFmpeg stderr to a file for debugging (was /dev/null — invisible failures)
    let ffmpeg_log_path = media_dir.parent()
        .and_then(|_| dirs::home_dir())
        .map(|h| h.join(".spela").join("ffmpeg.log"))
        .unwrap_or_else(|| media_dir.join("ffmpeg.log"));
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
) -> Result<(PathBuf, u32)> {
    let hls_dir = media_dir.join("transcoded_hls");

    // Fresh play, fresh segments. Old segments + manifest get wiped here
    // rather than waiting for `do_cleanup` so we never serve stale content
    // from a prior play that didn't reach `do_cleanup`.
    let _ = std::fs::remove_dir_all(&hls_dir);
    std::fs::create_dir_all(&hls_dir)?;

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
        filter.push_str("[0:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v0]; ");
        filter.push_str("[0:a]aresample=48000[a0]; ");
        if has_subs {
            let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
                .replace(':', "\\:");
            filter.push_str(&format!(
                "[{}:v]subtitles='{}',scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v1]; ",
                main_idx, srt_str
            ));
        } else {
            filter.push_str(&format!(
                "[{}:v]scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v1]; ",
                main_idx
            ));
        }
        filter.push_str(&format!("[{}:a]aresample=48000[a1]; ", main_idx));
        filter.push_str("[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]");
        args.extend([
            "-filter_complex".into(), filter,
            "-map".into(), "[v]".into(),
            "-map".into(), "[a]".into(),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else if has_subs {
        let srt_str = subtitle_path.unwrap().to_string_lossy().to_string()
            .replace(':', "\\:");
        args.extend([
            "-vf".into(), format!("subtitles='{}'", srt_str),
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else if video_reencode {
        args.extend([
            "-c:v".into(), "h264_nvenc".into(),
            "-preset".into(), "p4".into(),
            "-cq".into(), "23".into(),
        ]);
    } else {
        // H.264 video can be stream-copied — fastest path
        args.extend(["-c:v".into(), "copy".into()]);
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
}
