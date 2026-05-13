use anyhow::Result;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub const MAX_MEDIA_MB: u64 = 10_000;

/// Match a filesystem entry name against an active-play title, tolerant to
/// separator variation (dots vs spaces vs underscores vs dashes). Used by
/// `prune_disk` / `prune_to_fit` to protect the currently-playing content
/// from eviction even when the torrent release name separates tokens with
/// dots (`The.Boys.S05E03.FLUX.mkv`) while the request title comes in as
/// dashed or spaced (`The Boys S05E03`). Empty `active_title` returns false
/// so callers can use it as the "protect nothing" sentinel.
fn title_matches_active(name: &str, active_title: &str) -> bool {
    if active_title.is_empty() {
        return false;
    }
    let tokenize = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect()
    };
    let name_tokens = tokenize(name);
    let active_tokens = tokenize(active_title);
    if active_tokens.is_empty() {
        return false;
    }
    active_tokens.iter().all(|t| name_tokens.contains(t))
}

/// Minimum free space (MB) on the host filesystem below which spela refuses
/// to start new downloads, regardless of whether its own `~/media/` cap is
/// satisfied. This is a second, independent safety floor that protects
/// Darwin's system-critical services — kamal-proxy, Docker builds, FalkorDB,
/// Graphiti, AdGuard, journald, etc. — from being starved by a runaway
/// torrent on a machine where spela is a low-priority tenant.
const MIN_FS_FREE_MB: u64 = 20 * 1024;

/// Check if new downloads are safe: media dir must be under its own cap,
/// AND the host filesystem must have enough free space that spela can't
/// put pressure on higher-priority services.
pub fn check_space(media_dir: &Path) -> Result<Option<String>> {
    if !media_dir.exists() {
        return Ok(None);
    }
    let size_bytes = dir_size(media_dir)?;
    let size_mb = size_bytes / (1024 * 1024);
    if size_mb > MAX_MEDIA_MB {
        return Ok(Some(format!(
            "~/media/ is {}MB (>{}MB cap). Clean up first.",
            size_mb, MAX_MEDIA_MB
        )));
    }

    // Host-filesystem safety floor. `df` is present on every Unix spela
    // runs on (Ubuntu, macOS); if it's missing or its output is unparseable
    // we treat the check as unenforceable rather than refusing the play,
    // otherwise spela becomes unusable on the slightest parser regression.
    if let Some(free_mb) = fs_free_mb(media_dir) {
        if free_mb < MIN_FS_FREE_MB {
            return Ok(Some(format!(
                "Host filesystem has only {}MB free (<{}MB safety floor); spela is backing off to keep Darwin's critical services unaffected.",
                free_mb, MIN_FS_FREE_MB
            )));
        }
    }

    Ok(None)
}

/// Query the host filesystem free space (MB) via `df -Pk <path>`. Returns
/// `None` on any failure (missing binary, non-zero exit, parse error, etc.)
/// so callers can treat the safety floor as best-effort.
fn fs_free_mb(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["-Pk"]) // POSIX output (no line wrapping), 1K blocks
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    // Header is line 0; data line is line 1 in POSIX mode (no wrap).
    let data = text.lines().nth(1)?;
    // Columns: Filesystem, 1K-blocks, Used, Available, Capacity, Mounted-on
    let cols: Vec<&str> = data.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    let available_kb: u64 = cols[3].parse().ok()?;
    Some(available_kb / 1024)
}

/// Smart Disk Hygiene: age-based prune of media_dir entries.
///
/// - Active title: NEVER touched (contains match). If `active_title` is empty,
///   NOTHING is protected via title match — use an explicit non-empty sentinel
///   if you need to protect something. Empty active_title used to be a silent
///   no-op because `"anything".contains("")` matches, so `prune_disk` skipped
///   every entry. Apr 15, 2026 fix: the empty-string contains check is
///   short-circuited so an empty active_title means "protect nothing".
/// - Top-level files: handled same as directories (Apr 15, 2026 fix — earlier
///   versions only walked directories, so single-file torrents at the media
///   root like `Movie.2026.1080p.FLUX.mkv` were immortal).
/// - Completed (directory containing `.spela_done`): deleted after 7 days.
/// - Top-level single-file releases: treated as completed once present (no
///   `.spela_done` marker exists for files), so the 7-day grace applies.
/// - Partial / in-progress directories: deleted after 24h of inactivity.
pub fn prune_disk(media_dir: &Path, active_title: &str) {
    if !media_dir.exists() {
        return;
    }
    let now = SystemTime::now();

    let entries = match std::fs::read_dir(media_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Protect the active title — token-based match so dot-separated
        // torrent filenames (`The.Boys.S05E03.FLUX.mkv`) line up with
        // space-separated request titles (`The Boys S05E03`). Empty
        // active_title protects nothing (was a silent no-op before Apr 15).
        if title_matches_active(&name, active_title) {
            continue;
        }

        // Apr 30, 2026 (M9): refuse to delete symlinks. media_dir is
        // canonicalized at do_play but symlinks WITHIN can still escape
        // (a symlink at ~/media/foo -> ~/Documents would let prune nuke
        // user data). Skip symlinks entirely; remove_dir_all only acts on
        // physical entries owned by the torrent client.
        if let Ok(ft) = entry.file_type() {
            if ft.is_symlink() {
                tracing::debug!("prune_disk: skipping symlink {:?}", path);
                continue;
            }
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let age = match now.duration_since(modified) {
            Ok(a) => a,
            Err(_) => continue,
        };

        let is_dir = path.is_dir();
        let is_completed_dir = is_dir && path.join(".spela_done").exists();
        // Top-level files are treated as completed (no per-file done marker
        // is ever written — the torrent's output on a single-file release
        // IS the bypass cache), so they get the same 7-day grace as
        // completed season-pack folders.
        let is_completed_file = !is_dir;
        let max_age = if is_completed_dir || is_completed_file {
            Duration::from_secs(7 * 24 * 3600)
        } else {
            Duration::from_secs(24 * 3600)
        };

        if age > max_age {
            tracing::info!(
                "Smart Disk Hygiene: pruning '{}' (age {}h, {}, age-based)",
                name,
                age.as_secs() / 3600,
                if is_dir {
                    if is_completed_dir {
                        "completed-dir"
                    } else {
                        "partial-dir"
                    }
                } else {
                    "file"
                }
            );
            if is_dir {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Self-healing cap enforcement: run age-based `prune_disk` first, then if
/// `media_dir` is still over `target_mb`, evict the oldest entries (LRU by
/// mtime) until under cap, always skipping the currently-active title.
///
/// This turns `MAX_MEDIA_MB` from a REFUSAL wall into a SELF-MAINTAINING
/// upper bound: instead of `spela play` failing with "~/media/ is 13GB > 10GB
/// cap" when the user has been watching lots of content, spela silently
/// evicts the oldest cached torrent to make room.
///
/// Apr 15, 2026: added after a debug session where 8-day-old `28 Years Later`
/// files + multiple 4GB The Boys episodes filled the cache to 13GB and every
/// new play bounced off the 10GB cap. The old prune_disk couldn't help
/// because (a) top-level files were ignored and (b) the recent episodes
/// were under the 24h/7d age thresholds. LRU pressure eviction breaks both
/// of those stalemates.
pub fn prune_to_fit(media_dir: &Path, active_title: &str, target_mb: u64) {
    if !media_dir.exists() {
        return;
    }
    // Phase 1: age-based cleanup (fast, removes obviously stale entries).
    prune_disk(media_dir, active_title);

    // Phase 2: if still over target, LRU-evict.
    //
    // Apr 30, 2026 (M2 perf): pre-fix this re-read + re-sorted on every
    // iteration → O(N² log N) for N entries. Now collects+sorts once,
    // iterates the sorted list evicting until under cap, with dir_size
    // recheck per eviction (the recheck is cheap; the read_dir+sort were
    // the costly steps). Trade-off: candidates added DURING the eviction
    // loop aren't seen this pass — fine because prune_to_fit is best-effort
    // and runs on every do_play.
    let initial_mb = match dir_size(media_dir) {
        Ok(bytes) => bytes / (1024 * 1024),
        Err(_) => return,
    };
    if initial_mb <= target_mb {
        return;
    }

    let entries_iter = match std::fs::read_dir(media_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut candidates: Vec<(SystemTime, std::path::PathBuf)> = entries_iter
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if title_matches_active(&name, active_title) {
                return None;
            }
            // M9 (Apr 30): refuse to evict symlinks — same defense as
            // prune_disk, prevents escape from media_dir.
            if let Ok(ft) = entry.file_type() {
                if ft.is_symlink() {
                    return None;
                }
            }
            // Preserve tiny subtitle / helper files — they aren't meaningful
            // cache pressure and their mtime can be misleading.
            if !path.is_dir() {
                if let Ok(meta) = entry.metadata() {
                    if meta.len() < 5 * 1024 * 1024 {
                        return None;
                    }
                }
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();

    if candidates.is_empty() {
        tracing::warn!(
            "prune_to_fit: media_dir {}MB > target {}MB but no prunable entries left",
            initial_mb,
            target_mb
        );
        return;
    }
    candidates.sort_by_key(|(t, _)| *t);

    // Evict oldest-first until under cap or candidates exhausted.
    for (mtime, path) in candidates {
        let current_mb = match dir_size(media_dir) {
            Ok(bytes) => bytes / (1024 * 1024),
            Err(_) => return,
        };
        if current_mb <= target_mb {
            return;
        }
        let age_h = SystemTime::now()
            .duration_since(mtime)
            .ok()
            .map(|d| d.as_secs() / 3600)
            .unwrap_or(0);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        tracing::info!(
            "prune_to_fit: LRU-evicting '{}' (age {}h) to bring media_dir under {}MB cap",
            name,
            age_h,
            target_mb
        );
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    // After exhausting candidates, re-check and warn if still over.
    if let Ok(bytes) = dir_size(media_dir) {
        let final_mb = bytes / (1024 * 1024);
        if final_mb > target_mb {
            tracing::warn!(
                "prune_to_fit: exhausted candidates with media_dir at {}MB > target {}MB",
                final_mb,
                target_mb
            );
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
        // The 10 GB media cap will never fire on an empty dir — but the
        // 20 GB host-filesystem safety floor is environment-dependent and
        // CAN fire on a tight test host (e.g. Mac Mini at 20 GiB free).
        // Accept either None (host has plenty of free space) or a floor
        // warning, but reject any other surprise warning text.
        let warning = result.unwrap();
        if let Some(w) = warning {
            assert!(
                w.contains("Host filesystem") && w.contains("safety floor"),
                "unexpected check_space warning: {}",
                w
            );
        }
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
        assert_eq!(
            size % 512,
            0,
            "dir_size should be block-aligned (got {size})"
        );
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
        assert_eq!(
            size % 512,
            0,
            "dir_size should be block-aligned (got {size})"
        );
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
        assert_eq!(std::fs::metadata(&sparse).unwrap().len(), 100 * 1024 * 1024);

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

    /// Apr 15, 2026 regression: `prune_disk` only handled directories and
    /// silently skipped top-level files. Single-file torrent releases like
    /// `The.Boys.S05E01.FLUX.mkv` and `28.Years.Later.mkv` were immortal.
    #[test]
    fn test_prune_disk_handles_top_level_files() {
        let dir = tempdir("prune_top_level_files");
        let old_file = dir.join("Ancient.Movie.2018.FLUX.mkv");
        fs::write(&old_file, vec![0u8; 1024]).unwrap();
        // Back-date to 10 days ago (past the 7-day "completed-file" threshold).
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        filetime::set_file_mtime(
            &old_file,
            filetime::FileTime::from_system_time(ten_days_ago),
        )
        .ok();

        prune_disk(&dir, "NotMatching");
        assert!(
            !old_file.exists(),
            "10-day-old top-level file should have been pruned"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Recent top-level file (within 7 days) must survive — don't get
    /// overzealous and nuke fresh content.
    #[test]
    fn test_prune_disk_preserves_recent_top_level_file() {
        let dir = tempdir("prune_top_level_recent");
        let fresh = dir.join("Recent.Episode.2026.mkv");
        fs::write(&fresh, vec![0u8; 1024]).unwrap();
        prune_disk(&dir, "NotMatching");
        assert!(fresh.exists(), "fresh file must not be pruned");
        fs::remove_dir_all(&dir).ok();
    }

    /// The Apr 15 root-cause bug: `prune_disk(dir, "")` used to be a silent
    /// no-op because `"any".contains("")` matches every name, so every entry
    /// was skipped. A test that actually CALLS the prune path on an aged
    /// entry pins the fix.
    #[test]
    fn test_prune_disk_empty_active_title_still_prunes() {
        let dir = tempdir("prune_empty_active");
        let old_file = dir.join("Old.Movie.mkv");
        fs::write(&old_file, vec![0u8; 1024]).unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        filetime::set_file_mtime(
            &old_file,
            filetime::FileTime::from_system_time(ten_days_ago),
        )
        .ok();

        // Empty active_title used to protect everything. Now it protects nothing.
        prune_disk(&dir, "");
        assert!(
            !old_file.exists(),
            "empty active_title must not shield old files from pruning"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Active title protects matching entries even when they're ancient.
    #[test]
    fn test_prune_disk_respects_active_title() {
        let dir = tempdir("prune_active_protected");
        let active = dir.join("The.Boys.S05E03.mkv");
        fs::write(&active, vec![0u8; 1024]).unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        filetime::set_file_mtime(&active, filetime::FileTime::from_system_time(ten_days_ago)).ok();

        prune_disk(&dir, "The Boys S05E03");
        assert!(
            active.exists(),
            "file matching active_title must survive pruning"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `prune_to_fit` must evict oldest entries first until under the target
    /// cap, even when all entries are under the age-based threshold. This
    /// is the scenario that bit us on Apr 15: three recent 4GB episodes
    /// over the 10GB cap, none old enough for age-based eviction.
    #[test]
    fn test_prune_to_fit_lru_eviction() {
        let dir = tempdir("prune_to_fit_lru");

        // Create three 1MB files with staggered mtimes. The LRU (oldest)
        // should get evicted first.
        let now = SystemTime::now();
        let make = |name: &str, age_secs: u64, size_mb: usize| {
            let p = dir.join(name);
            fs::write(&p, vec![0u8; size_mb * 1024 * 1024]).unwrap();
            let mtime = now - Duration::from_secs(age_secs);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mtime)).ok();
            p
        };
        let oldest = make("oldest.mkv", 3600, 6); // 6 MB, 1h old
        let middle = make("middle.mkv", 1800, 6); // 6 MB, 30 min old
        let newest = make("newest.mkv", 60, 6); // 6 MB, 1 min old

        // Cap at 15 MB — we have 18 MB, need to drop 3+ MB → evict oldest.
        prune_to_fit(&dir, "nothing-matches", 15);

        assert!(!oldest.exists(), "oldest file should be LRU-evicted");
        assert!(middle.exists(), "middle file should survive");
        assert!(newest.exists(), "newest file should survive");
        fs::remove_dir_all(&dir).ok();
    }

    /// `prune_to_fit` must never evict an entry matching the active title,
    /// even if it's the oldest.
    #[test]
    fn test_prune_to_fit_protects_active_title() {
        let dir = tempdir("prune_to_fit_active");
        let now = SystemTime::now();
        let active = {
            let p = dir.join("The.Boys.S05E03.mkv");
            fs::write(&p, vec![0u8; 6 * 1024 * 1024]).unwrap();
            let mtime = now - Duration::from_secs(3600);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mtime)).ok();
            p
        };
        let newer = {
            let p = dir.join("Recent.Movie.mkv");
            fs::write(&p, vec![0u8; 6 * 1024 * 1024]).unwrap();
            let mtime = now - Duration::from_secs(60);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mtime)).ok();
            p
        };
        prune_to_fit(&dir, "The Boys S05E03", 10);
        assert!(active.exists(), "active title must NEVER be evicted");
        // Newer must have been evicted since active is protected
        assert!(
            !newer.exists(),
            "newer must be evicted when active is protected"
        );
        fs::remove_dir_all(&dir).ok();
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spela_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir); // clean any stale dir
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
