use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Detect audio codec of a media URL/file using ffprobe.
pub async fn detect_audio_codec(url: &str) -> Result<Option<String>> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_name", url])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    Ok(combined.lines()
        .find_map(|line| line.strip_prefix("codec_name="))
        .map(|s| s.trim().to_string()))
}

/// Returns true if the codec needs transcoding for Chromecast.
pub fn needs_transcode(codec: &str) -> bool {
    matches!(codec, "ac3" | "eac3" | "dts" | "truehd" | "dca")
}

/// Transcode audio to AAC from an HTTP URL (progressive/streaming input).
/// Uses -re (real-time read) so ffmpeg never outruns the download.
/// Uses reconnect flags so temporary download stalls don't cause EOF.
/// Optionally burns in subtitles if subtitle_path is provided.
/// Returns (output_path, ffmpeg_pid).
pub async fn transcode_audio(
    input_url: &str,
    media_dir: &Path,
    subtitle_path: Option<&Path>,
) -> Result<(PathBuf, u32)> {
    let output_path = media_dir.join("transcoded_aac.mp4");

    let mut args: Vec<String> = vec![
        // Input: read at real-time speed so we never outrun the download
        "-re".into(),
        // Reconnect on stalls/drops — keeps the stream alive during slow periods
        "-reconnect".into(), "1".into(),
        "-reconnect_at_eof".into(), "1".into(),
        "-reconnect_streamed".into(), "1".into(),
        "-reconnect_delay_max".into(), "30".into(),
        // HTTP read timeout: 60 seconds of silence before giving up (microseconds)
        "-rw_timeout".into(), "60000000".into(),
        "-i".into(), input_url.into(),
    ];

    // Subtitle burn-in: if we have an SRT file, hardcode it into the video
    // This requires video re-encoding (NVENC on Darwin's GTX 1650)
    if let Some(srt_path) = subtitle_path {
        if srt_path.exists() {
            let srt_str = srt_path.to_string_lossy().to_string()
                .replace(':', "\\:"); // ffmpeg subtitle filter needs escaped colons
            args.extend([
                "-vf".into(), format!("subtitles='{}'", srt_str),
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p4".into(), // balanced speed/quality
                "-cq".into(), "23".into(),     // constant quality
            ]);
        } else {
            args.extend(["-c:v".into(), "copy".into()]);
        }
    } else {
        args.extend(["-c:v".into(), "copy".into()]);
    }

    args.extend([
        "-c:a".into(), "aac".into(),
        "-ac".into(), "2".into(),
        "-b:a".into(), "192k".into(),
        "-movflags".into(), "frag_keyframe+empty_moov+default_base_moof".into(),
        "-y".into(),
        output_path.to_str().unwrap_or("transcoded_aac.mp4").into(),
    ]);

    let child = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let pid = child.id().unwrap_or(0);

    // Detach — we track by PID, not by Child handle
    std::mem::forget(child);

    Ok((output_path, pid))
}
