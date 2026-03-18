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

/// Find the largest video file in the media directory (the downloaded torrent file).
/// Waits up to `wait_secs` for the file to stabilize (stop growing = fully downloaded).
pub async fn find_local_video(media_dir: &Path, wait_secs: u64) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, u64)> = None;

    fn scan_dir(dir: &Path, best: &mut Option<(PathBuf, u64)>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan_dir(&path, best);
                } else if path.is_file() {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "mp4" | "mkv" | "avi" | "webm") {
                        if let Ok(meta) = std::fs::metadata(&path) {
                            if best.as_ref().map_or(true, |(_, s)| meta.len() > *s) {
                                *best = Some((path, meta.len()));
                            }
                        }
                    }
                }
            }
        }
    }

    // Wait for file to finish downloading (size stabilizes)
    for _ in 0..wait_secs {
        best = None;
        scan_dir(media_dir, &mut best);
        if let Some((ref path, size)) = best {
            if size > 0 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                // Check if size changed
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.len() == size {
                        // Size stable — file is complete
                        return Some(path.clone());
                    }
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    // Return whatever we found even if still growing
    best.map(|(p, _)| p)
}

/// Transcode audio to AAC, copy video. Uses local file path, not HTTP URL.
/// This avoids EOF truncation when ffmpeg outruns the download.
pub async fn transcode_audio(
    input: &str,
    media_dir: &Path,
) -> Result<(PathBuf, tokio::process::Child)> {
    let output_path = media_dir.join("transcoded_aac.mp4");

    let child = Command::new("ffmpeg")
        .args([
            "-i", input,
            "-c:v", "copy",
            "-c:a", "aac",
            "-ac", "2",
            "-b:a", "192k",
            "-movflags", "frag_keyframe+empty_moov+default_base_moof",
            "-y",
            output_path.to_str().unwrap_or("transcoded_aac.mp4"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    Ok((output_path, child))
}
