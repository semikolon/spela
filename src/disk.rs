use anyhow::Result;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

const MAX_MEDIA_MB: u64 = 10000;

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

/// Smart Disk Hygiene: Prune folders based on age and completion status.
/// - Active movie: NEVER touched.
/// - Partial downloads: Deletes after 24h of inactivity.
/// - Completed (.spela_done): Deletes after 7 days to allow re-watching.
pub fn prune_disk(media_dir: &Path, active_title: &str) {
    if !media_dir.exists() { return; }
    let now = SystemTime::now();

    if let Ok(entries) = std::fs::read_dir(media_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }

            let name = entry.file_name().to_string_lossy().to_string();
            // SKIP the active movie to prevent stream termination
            if name.contains(active_title) { continue; }

            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(age) = now.duration_since(modified) {
                        let is_done = path.join(".spela_done").exists();
                        let max_age = if is_done {
                            Duration::from_secs(7 * 24 * 3600) // 7 days grace for completed
                        } else {
                            Duration::from_secs(24 * 3600) // 24 hours for partial/stale
                        };

                        if age > max_age {
                            tracing::info!("Smart Disk Hygiene: Pruning '{}' (Age: {}h, Done: {})", name, age.as_secs() / 3600, is_done);
                            let _ = std::fs::remove_dir_all(&path);
                        }
                    }
                }
            }
        }
    }
}

/// Backward-compatible wrapper for the older cleanup test. New production code
/// should use `prune_disk` so the active title can be protected explicitly.
#[cfg(test)]
fn cleanup_old_files(media_dir: &Path) {
    prune_disk(media_dir, "");
}

/// Recursively sum the real on-disk usage of a path tree.
///
/// Critically this uses allocated blocks (`metadata.blocks() * 512`), not
/// the logical `len()`. webtorrent-cli with `-s <file_index>` creates sparse
/// placeholder files for the unselected files in a torrent: those placeholders
/// report the full torrent file size via `metadata.len()` even though only a
/// handful of downloaded pieces actually occupy the disk. Using `len()` made
/// `check_space()` trip the 10GB cap before the torrent had downloaded
/// anything — spela would refuse to start new plays with a bogus "disk full"
/// error while `du -sh ~/media/` reported only a few GB of real usage.
pub fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_file() {
        // Unix reports allocated storage in 512-byte blocks regardless of the
        // filesystem's own block size, so this is the correct conversion.
        total += path.metadata()?.blocks() * 512;
    } else if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            total += dir_size(&entry?.path())?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_check_space_nonexistent_dir() {
        let result = check_space(Path::new("/tmp/spela_test_nonexistent_dir_12345"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_check_space_empty_dir() {
        let dir = tempdir("check_space_empty");
        let result = check_space(&dir);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none()); // 0 bytes < 10GB
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_dir_size_with_files() {
        let dir = tempdir("dir_size_files");
        fs::write(dir.join("a.txt"), "hello").unwrap();
        fs::write(dir.join("b.txt"), "world!").unwrap();
        // `dir_size` reports allocated bytes (blocks * 512), so tiny files
        // round up to the filesystem's minimum allocation (usually 4KB on
        // APFS / ext4). We just need the result to be block-aligned and
        // at least as big as the logical content.
        let size = dir_size(&dir).unwrap();
        assert!(size >= 11, "dir_size should be >= logical sum (got {size})");
        assert_eq!(size % 512, 0, "dir_size should be block-aligned (got {size})");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_dir_size_recursive() {
        let dir = tempdir("dir_size_recursive");
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(dir.join("root.txt"), "abc").unwrap();
        fs::write(sub.join("child.txt"), "defgh").unwrap();
        let size = dir_size(&dir).unwrap();
        assert!(size >= 8, "dir_size should be >= logical sum (got {size})");
        assert_eq!(size % 512, 0, "dir_size should be block-aligned (got {size})");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_dir_size_counts_sparse_file_as_allocated_not_logical() {
        // Regression: webtorrent with `-s <file_index>` creates sparse
        // placeholder files for the unselected files in a torrent — they
        // report the full torrent file size via metadata.len() but occupy
        // almost nothing on disk. Before switching to blocks, dir_size
        // summed those logical sizes and tripped the 10GB cap before any
        // real download had happened.
        let dir = tempdir("dir_size_sparse");
        let sparse = dir.join("sparse.bin");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&sparse)
            .unwrap();
        // 100 MB logical, zero actual bytes written.
        f.set_len(100 * 1024 * 1024).unwrap();
        drop(f);

        // Sanity: the OS reports the logical length as 100 MB.
        assert_eq!(
            std::fs::metadata(&sparse).unwrap().len(),
            100 * 1024 * 1024
        );

        // But dir_size must report nearly nothing — the sparse file should
        // occupy at most a few filesystem blocks of metadata, far below
        // the 100 MB the naive len() implementation would have returned.
        let size = dir_size(&dir).unwrap();
        assert!(
            size < 10 * 1024 * 1024,
            "sparse file must not count its logical size (got {size})"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_old_files_preserves_new() {
        let dir = tempdir("cleanup_preserves");
        fs::write(dir.join("new.txt"), "keep me").unwrap();
        cleanup_old_files(&dir);
        assert!(dir.join("new.txt").exists());
        fs::remove_dir_all(&dir).ok();
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spela_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir); // clean any stale dir
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
