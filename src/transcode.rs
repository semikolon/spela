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

/// Transcode audio to AAC, copy video. Returns path to transcoded file.
/// Runs ffmpeg in background — caller should wait a few seconds before casting.
pub async fn transcode_audio(
    input_url: &str,
    media_dir: &Path,
) -> Result<(PathBuf, tokio::process::Child)> {
    let output_path = media_dir.join("transcoded_aac.mp4");

    let child = Command::new("ffmpeg")
        .args([
            "-i", input_url,
            "-c:v", "copy",
            "-c:a", "aac",
            "-ac", "2",
            "-b:a", "192k",
            "-movflags", "+faststart",
            "-y",
            output_path.to_str().unwrap_or("transcoded_aac.mp4"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    Ok((output_path, child))
}
