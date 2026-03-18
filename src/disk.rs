use anyhow::Result;
use std::path::Path;
use std::time::{Duration, SystemTime};

const MAX_MEDIA_MB: u64 = 10000;
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Check if media dir exceeds 10GB cap.
pub fn check_space(media_dir: &Path) -> Result<Option<String>> {
    if !media_dir.exists() {
        return Ok(None);
    }
    let size_bytes = dir_size(media_dir)?;
    let size_mb = size_bytes / (1024 * 1024);
    if size_mb > MAX_MEDIA_MB {
        Ok(Some(format!("~/media/ is {}MB (>{}MB cap). Clean up first.", size_mb, MAX_MEDIA_MB)))
    } else {
        Ok(None)
    }
}

/// Delete files older than 24h in media dir.
pub fn cleanup_old_files(media_dir: &Path) {
    if !media_dir.exists() { return; }
    let now = SystemTime::now();
    if let Ok(entries) = std::fs::read_dir(media_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    if let Ok(modified) = meta.modified() {
                        if let Ok(age) = now.duration_since(modified) {
                            if age > MAX_AGE {
                                let _ = std::fs::remove_file(entry.path());
                            }
                        }
                    }
                }
            }
        }
    }
    // Remove empty subdirectories
    if let Ok(entries) = std::fs::read_dir(media_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    let _ = std::fs::remove_dir(entry.path()); // only removes if empty
                }
            }
        }
    }
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += dir_size(&entry.path())?;
            }
        }
    }
    Ok(total)
}
