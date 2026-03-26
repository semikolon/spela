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

/// Transcode media from an HTTP URL (progressive/streaming input).
/// Uses -re (real-time read) so ffmpeg never outruns the download.
/// Uses reconnect flags so temporary download stalls don't cause EOF.
/// Optionally re-encodes video (HEVC→H.264), burns in subtitles, and/or prepends intro clip.
/// Returns (output_path, ffmpeg_pid).
pub async fn transcode(
    input_url: &str,
    media_dir: &Path,
    subtitle_path: Option<&Path>,
    intro_path: Option<&Path>,
    video_reencode: bool,
) -> Result<(PathBuf, u32)> {
    let output_path = media_dir.join("transcoded_aac.mp4");
    let has_intro = intro_path.is_some();
    let has_subs = subtitle_path.map_or(false, |p| p.exists());

    let mut args: Vec<String> = Vec::new();

    // Input 0: intro clip (if present, no -re — plays at full speed)
    if let Some(intro) = intro_path {
        args.extend(["-i".into(), intro.to_string_lossy().to_string()]);
    }

    // Input 1 (or 0 if no intro): main stream with real-time pacing
    args.extend([
        "-re".into(),
        "-reconnect".into(), "1".into(),
        "-reconnect_at_eof".into(), "1".into(),
        "-reconnect_streamed".into(), "1".into(),
        "-reconnect_delay_max".into(), "30".into(),
        "-rw_timeout".into(), "60000000".into(),
        "-i".into(), input_url.into(),
    ]);

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

    tracing::debug!("ffmpeg args: {:?}", args);

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
