//! HLS cache for fully-transcoded episodes (v3.5.0, May 13, 2026).
//!
//! # Problem
//!
//! Local Bypass plays (resuming a watched episode with the source MKV on
//! disk) historically wait ~150-200 s for ffmpeg to transcode the remaining
//! content into HLS before LOAD, because spela's `Chromecast reliability
//! mode` (`should_wait_for_complete_hls_before_cast`) blocks on the
//! `#EXT-X-ENDLIST` marker for cast stability on CrKey 1.56. The transcode
//! work is real — for a 60 min episode resumed at minute 50, ffmpeg has to
//! re-encode the final 10 min at ~4× realtime = ~150 s wall.
//!
//! Every resume / replay of the same episode pays this cost again because
//! `do_cleanup` deletes the transcoded HLS dir on playback end.
//!
//! # Solution
//!
//! Persist the transcoded HLS set across plays. After ffmpeg natural-exits
//! from a full-episode transcode (`ss_offset == 0.0`, exit code 0), atomically
//! promote the active dir to `<media_dir>/hls_cache/<key>/`. Subsequent plays
//! of the same episode short-circuit the entire torrent + ffmpeg pipeline:
//! LOAD the cached master playlist directly on Chromecast, then `cast.seek`
//! to the resume position post-LOAD (Default Media Receiver's native seek on
//! VOD HLS with `#EXT-X-ENDLIST` works reliably on CrKey 1.56).
//!
//! # Cache key
//!
//! `<imdb_id>_<sxxeyy>_<lang>_<intro>_v<CACHE_VERSION>`
//!
//! Components capture every transcode-output-affecting setting:
//! - `imdb_id` + `season` + `episode`: identifies the episode
//! - `subtitle_lang`: subs are burned in (different lang → different output)
//! - `has_intro`: intro clip is concatenated (changes timeline + output)
//! - `CACHE_VERSION`: bumps when transcode params change (codec, ladder,
//!   bitrate, NVENC settings); old cached sets become unhittable
//!
//! Episodes without `imdb_id` (raw magnet plays, search-less plays) are NOT
//! cached — `build_cache_key` returns `None`.
//!
//! # Lifecycle
//!
//! 1. **Cache check** (in `do_play`, before Local Bypass + torrent): if
//!    [`is_cache_hit`] returns true for the computed key, skip ffmpeg + torrent
//!    entirely; cast the cached manifest directly with `cast.seek(resume_pos)`
//!    post-LOAD.
//! 2. **Cache miss** (no cache or partial fill): existing flow runs.
//!    `transcode_hls` writes segments to the active `transcoded_hls/` dir.
//! 3. **Cache fill on natural exit** (in the post-playback reaper): when
//!    ffmpeg exits cleanly AND the source play was `ss_offset == 0.0`,
//!    atomically rename the active dir to `<cache_root>/<key>/` and write
//!    [`COMPLETE_MARKER`] inside it. Partial transcodes (user stopped early)
//!    and resume transcodes (`ss_offset > 0.0`) are NOT cached.
//! 4. **LRU eviction**: when the cache dir's total size exceeds the configured
//!    `hls_cache_cap_mb`, [`prune_cache_to_fit`] evicts oldest-mtime entries
//!    until under cap. Runs on cache fill + on `do_play` startup as a safety
//!    pass.
//!
//! # Atomicity
//!
//! Cache hits MUST be all-or-nothing: serving a partial set to Chromecast
//! produces the live-edge-HLS pathologies the cache is meant to AVOID.
//! [`COMPLETE_MARKER`] is the sentinel: only after `mark_complete` has
//! written it is the cache dir considered hittable. The rename from
//! the active transcode dir uses [`std::fs::rename`] which is atomic within
//! a single filesystem.
//!
//! # Non-goals (v3.5.0)
//!
//! - **Background transcode-ahead**: when user stops at minute 5, finish
//!   transcoding the rest in the background to populate the cache. Useful
//!   for episodes that get re-watched without ever being fully watched the
//!   first time. Deferred to v3.6.0 (cost: extra ffmpeg subprocess; complexity:
//!   queue/scheduling, GPU contention with active plays).
//! - **Cross-quality cache**: separate cache entries for 720p-only vs
//!   1080p+480p adaptive ladders. Currently all transcodes produce the same
//!   ladder, so this is moot until a quality-config knob exists.
//! - **Distributed cache**: sharing the cache across multiple spela hosts
//!   (e.g., Mac + Darwin). Out of scope.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Cache schema version. Bump when transcode params change so old cached
/// sets are no longer hit. Stale caches are LRU-evicted naturally.
pub const CACHE_VERSION: u32 = 1;

/// Subdirectory under `media_dir` where cache entries live.
pub const CACHE_DIR_NAME: &str = "hls_cache";

/// Sentinel filename written atomically inside a cache dir after ffmpeg
/// writes `#EXT-X-ENDLIST` and the dir has all expected segments.
/// [`is_cache_hit`] REQUIRES this file's existence — partial cache fills
/// are NOT served.
pub const COMPLETE_MARKER: &str = ".complete.v1";

/// Master playlist filename inside a cache dir. Cache hit requires this
/// file to exist alongside [`COMPLETE_MARKER`].
pub const MANIFEST_NAME: &str = "master.m3u8";

/// Build a deterministic cache key from the episode + render config.
///
/// Returns `None` when there isn't enough metadata to construct a stable
/// key — specifically when `imdb_id` is missing. Raw-magnet plays and
/// search-less plays don't get cached (the user can replay them but won't
/// benefit from the cache).
///
/// Key format: `<imdb_id>_<sxxeyy>_<lang>_<intro>_v<CACHE_VERSION>`. Example:
/// `tt1399664_s02e04_eng_intro_v1` for The Night Manager S02E04 with English
/// subs and intro clip; `tt1190634_s05e07_eng_nointro_v1` for The Boys S05E07
/// without intro. Movies (no season/episode) use `s00e00`.
///
/// Components are joined with `_` separator and lowercased. Spaces in
/// `imdb_id` (shouldn't happen, but defensive) are replaced with `-`.
pub fn build_cache_key(
    imdb_id: Option<&str>,
    season: Option<u32>,
    episode: Option<u32>,
    subtitle_lang: Option<&str>,
    has_intro: bool,
) -> Option<String> {
    let imdb = imdb_id?.trim();
    if imdb.is_empty() {
        return None;
    }
    // Defensive: enforce IMDb-ID format (tt followed by ≥1 digits) so a
    // malformed value can't create surprising filesystem paths. The `len > 2`
    // check rejects bare "tt" — `chars().all()` is vacuously true on an
    // empty suffix and would otherwise pass.
    if !imdb.starts_with("tt") || imdb.len() <= 2 || !imdb[2..].chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let s = season.unwrap_or(0);
    let e = episode.unwrap_or(0);
    let lang = subtitle_lang.unwrap_or("none");
    let lang_safe: String = lang
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let lang_safe = if lang_safe.is_empty() {
        "none".into()
    } else {
        lang_safe
    };
    let intro = if has_intro { "intro" } else { "nointro" };
    Some(format!(
        "{}_s{:02}e{:02}_{}_{}_v{}",
        imdb, s, e, lang_safe, intro, CACHE_VERSION
    ))
}

/// Cache key for a LIBRARY / Local-Bypass play that has no `imdb_id`
/// (the v3.7.1 title-only bridge path → [`build_cache_key`] returns
/// `None` → such plays never cached: the 5th facet of the library-bridge
/// root). Keys off the raw library name instead so My-Library replays
/// can hit the cache.
///
/// FNV-1a 64-bit of the trimmed `raw_name` → 16-hex — deterministic,
/// dependency-free, collision-negligible at single-user library scale,
/// and hex is filesystem-safe (a path-traversal-y raw_name can never
/// leak `/` or `..` into the on-disk cache path). `lib`-prefixed so it
/// can't collide with an `tt…` imdb key. Returns `None` for an empty
/// name. Movies have no season/episode — the raw_name disambiguates
/// fully (including any embedded SxxExx).
pub fn build_cache_key_for_title(
    raw_name: &str,
    subtitle_lang: Option<&str>,
    has_intro: bool,
) -> Option<String> {
    let name = raw_name.trim();
    if name.is_empty() {
        return None;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let lang = subtitle_lang.unwrap_or("none");
    let lang_safe: String = lang
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let lang_safe = if lang_safe.is_empty() {
        "none".into()
    } else {
        lang_safe
    };
    let intro = if has_intro { "intro" } else { "nointro" };
    Some(format!(
        "lib{:016x}_{}_{}_v{}",
        h, lang_safe, intro, CACHE_VERSION
    ))
}

/// Resolve the cache key for a play. imdb-based when available
/// (search/torrent plays), else the raw-title hash (library/bypass
/// plays — the 5th-facet path). `None` when caching is disabled
/// (`cap_mb == 0`) so every caller gets ONE uniform "no key" signal.
///
/// SINGLE SOURCE OF TRUTH for BOTH cache-fill (`do_cleanup` promote)
/// AND cache-hit (`do_play` short-circuit): if these two computed the
/// key differently, a library play would fill under one key and miss
/// under another → the cache would never hit for library plays. Always
/// route both through this.
pub fn resolve_cache_key(
    cap_mb: u64,
    imdb_id: Option<&str>,
    season: Option<u32>,
    episode: Option<u32>,
    subtitle_lang: Option<&str>,
    has_intro: bool,
    title: Option<&str>,
) -> Option<String> {
    if cap_mb == 0 {
        return None;
    }
    build_cache_key(imdb_id, season, episode, subtitle_lang, has_intro)
        .or_else(|| title.and_then(|t| build_cache_key_for_title(t, subtitle_lang, has_intro)))
}

/// Compute the on-disk path for a cache entry. `media_dir/hls_cache/<key>/`.
pub fn cache_dir_for_key(media_dir: &Path, key: &str) -> PathBuf {
    media_dir.join(CACHE_DIR_NAME).join(key)
}

/// Root cache directory (parent of all per-episode cache entries).
pub fn cache_root(media_dir: &Path) -> PathBuf {
    media_dir.join(CACHE_DIR_NAME)
}

/// Total duration (seconds) of a COMPLETE cached set, from the Σ#EXTINF
/// of its first media variant. A cache-HIT play has no source to
/// `ffprobe`, but the cast needs a duration to enter Buffered/seekable
/// mode — without it the Chromecast treats the stream as Live (no
/// seek = no resume, defeating the cache's whole point). The cached set
/// always carries `#EXT-X-ENDLIST`, so the EXTINF sum is exact. Reads
/// the master's FIRST variant URI (multi-variant → `stream_0.m3u8`),
/// the server-side twin of the web scrubber's frontier derivation.
/// `None` on any I/O / parse miss (caller falls back to req.duration).
pub fn cached_duration(media_dir: &Path, key: &str) -> Option<f64> {
    let dir = cache_dir_for_key(media_dir, key);
    let master = std::fs::read_to_string(dir.join(MANIFEST_NAME)).ok()?;
    let variant = master
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("playlist.m3u8");
    let pl = std::fs::read_to_string(dir.join(variant)).ok()?;
    let mut sum = 0.0_f64;
    for line in pl.lines() {
        if let Some(rest) = line.trim().strip_prefix("#EXTINF:") {
            sum += rest
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0);
        }
    }
    if sum > 0.0 {
        Some(sum)
    } else {
        None
    }
}

/// Cache hit if BOTH the complete-marker AND the master manifest exist.
/// Either alone is treated as miss (defensive — handles partial-fill
/// failure modes and migration from older cache schemas).
pub fn is_cache_hit(media_dir: &Path, key: &str) -> bool {
    let dir = cache_dir_for_key(media_dir, key);
    dir.join(COMPLETE_MARKER).exists() && dir.join(MANIFEST_NAME).exists()
}

/// Atomically mark a cache dir as complete by writing the sentinel marker.
/// Called from the post-playback reaper after a successful full-episode
/// transcode (`ss_offset == 0.0`, ffmpeg exit code 0) and the rename of the
/// active transcode dir into the cache root.
pub fn mark_complete(cache_dir: &Path) -> std::io::Result<()> {
    let marker = cache_dir.join(COMPLETE_MARKER);
    std::fs::write(&marker, b"v1\n")?;
    Ok(())
}

/// Sum the on-disk byte size of all files in a directory tree. Uses
/// `metadata.blocks() * 512` on Unix so sparse files (legacy librqbit
/// placeholders, qcow2 disks) are accounted by ALLOCATED bytes, not logical
/// length. Matches the convention in `disk.rs::dir_size`.
///
/// Returns 0 on any I/O error (lazy is fine — cache size is advisory for
/// LRU eviction, not a hard limit).
pub fn cache_dir_size_bytes(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    total = total.saturating_add(metadata.blocks().saturating_mul(512));
                }
                #[cfg(not(unix))]
                {
                    total = total.saturating_add(metadata.len());
                }
            } else if metadata.is_dir() {
                total = total.saturating_add(cache_dir_size_bytes(&path));
            }
        }
    }
    total
}

/// Iterate the cache root's top-level subdirs (one per cache entry), each
/// paired with its modification time. Returns entries sorted by mtime
/// ASCENDING (oldest first) so LRU eviction can drain from the front.
///
/// `mtime` is taken from the cache directory itself rather than from the
/// `COMPLETE_MARKER` file because `std::fs::rename` (used to promote the
/// active transcode dir into the cache) preserves the source's mtime, which
/// is the time of the last segment write — close enough to the cache-fill
/// completion time for LRU purposes.
///
/// Entries lacking metadata are skipped silently (no recoverable error
/// signal needed; LRU is best-effort).
pub fn cache_entries_by_age(cache_root: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut entries: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(cache_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_dir() {
                return None;
            }
            let mtime = metadata.modified().ok()?;
            Some((path, mtime))
        })
        .collect();
    entries.sort_by_key(|(_, mtime)| *mtime);
    entries
}

/// LRU eviction: walk cache entries oldest-first, deleting until total size
/// is ≤ `cap_bytes`. Returns the number of entries deleted.
///
/// Best-effort: I/O errors during delete are logged via `tracing::warn!` and
/// the entry is skipped (loop continues). A cache that's stuck above cap due
/// to permission errors won't crash spela, just won't shrink.
///
/// Does NOT delete the cache root itself, even if it ends up empty.
pub fn prune_cache_to_fit(cache_root: &Path, cap_bytes: u64) -> usize {
    let total = cache_dir_size_bytes(cache_root);
    if total <= cap_bytes {
        return 0;
    }
    let mut to_free = total - cap_bytes;
    let mut deleted = 0usize;
    for (path, _mtime) in cache_entries_by_age(cache_root) {
        if to_free == 0 {
            break;
        }
        let entry_size = cache_dir_size_bytes(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::info!(
                    "HLS cache LRU: evicted {:?} ({} MB)",
                    path,
                    entry_size / 1024 / 1024
                );
                deleted += 1;
                to_free = to_free.saturating_sub(entry_size);
            }
            Err(e) => {
                tracing::warn!(
                    "HLS cache LRU: failed to delete {:?}: {} — skipping",
                    path,
                    e
                );
            }
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread::sleep;
    use std::time::Duration;
    use tempfile::TempDir;

    // --- Cache key construction ---

    #[test]
    fn test_build_cache_key_tv_with_subs_and_intro() {
        assert_eq!(
            build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), true).as_deref(),
            Some("tt1399664_s02e04_eng_intro_v1")
        );
    }

    #[test]
    fn test_build_cache_key_tv_without_intro() {
        assert_eq!(
            build_cache_key(Some("tt1190634"), Some(5), Some(7), Some("eng"), false).as_deref(),
            Some("tt1190634_s05e07_eng_nointro_v1")
        );
    }

    #[test]
    fn test_build_cache_key_movie_uses_s00e00() {
        assert_eq!(
            build_cache_key(Some("tt0111161"), None, None, Some("eng"), false).as_deref(),
            Some("tt0111161_s00e00_eng_nointro_v1")
        );
    }

    #[test]
    fn test_build_cache_key_no_imdb_returns_none() {
        assert!(build_cache_key(None, Some(1), Some(1), Some("eng"), false).is_none());
        assert!(build_cache_key(Some(""), Some(1), Some(1), Some("eng"), false).is_none());
        assert!(build_cache_key(Some("   "), Some(1), Some(1), Some("eng"), false).is_none());
    }

    #[test]
    fn test_build_cache_key_rejects_malformed_imdb() {
        // Defensive: malformed imdb_ids can't escape the format we expect, so
        // we don't materialize a cache dir for them. They simply don't cache.
        assert!(
            build_cache_key(Some("not-an-imdb"), Some(1), Some(1), Some("eng"), false).is_none()
        );
        assert!(build_cache_key(Some("tt"), Some(1), Some(1), Some("eng"), false).is_none()); // empty after tt
        assert!(build_cache_key(Some("tt12abc"), Some(1), Some(1), Some("eng"), false).is_none()); // non-digit after tt
        assert!(
            build_cache_key(Some("../etc/passwd"), Some(1), Some(1), Some("eng"), false).is_none()
        );
    }

    #[test]
    fn test_build_cache_key_no_lang_uses_none() {
        assert_eq!(
            build_cache_key(Some("tt1234567"), Some(1), Some(1), None, false).as_deref(),
            Some("tt1234567_s01e01_none_nointro_v1")
        );
    }

    #[test]
    fn test_build_cache_key_sanitizes_lang() {
        // Defense in depth: lang strings going into the path get sanitized
        // to ASCII alphanumerics. Real-world subtitle_lang is "en"/"sv"/"off",
        // so this is hardening rather than feature.
        assert_eq!(
            build_cache_key(
                Some("tt1234567"),
                Some(1),
                Some(1),
                Some("en/../etc"),
                false
            )
            .as_deref(),
            Some("tt1234567_s01e01_enetc_nointro_v1")
        );
    }

    #[test]
    fn test_build_cache_key_deterministic_for_same_inputs() {
        // Pure function — same inputs MUST produce same output every call.
        // Reasonable invariant for any cache key.
        let k1 = build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), true).unwrap();
        let k2 = build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), true).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_build_cache_key_different_intro_different_keys() {
        let k_intro =
            build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), true).unwrap();
        let k_no_intro =
            build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), false).unwrap();
        assert_ne!(k_intro, k_no_intro);
    }

    #[test]
    fn test_build_cache_key_different_lang_different_keys() {
        let k_en =
            build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), false).unwrap();
        let k_sv =
            build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("swe"), false).unwrap();
        assert_ne!(k_en, k_sv);
    }

    // --- Library title-hash key (v3.7.8 — 5th facet of the v3.7.1 root:
    // library-bridge plays carry no imdb_id, so build_cache_key() returns
    // None and they never cache. build_cache_key_for_title() keys off the
    // raw library name instead so My-Library replays can hit the cache.) ---

    #[test]
    fn test_title_key_deterministic_and_prefixed() {
        let a = build_cache_key_for_title("Grosse.Pointe.Blank.1997.1080p", Some("eng"), false);
        let b = build_cache_key_for_title("Grosse.Pointe.Blank.1997.1080p", Some("eng"), false);
        assert_eq!(a, b, "same raw_name+lang+intro must be deterministic");
        assert!(
            a.as_deref().unwrap().starts_with("lib"),
            "library keys are lib-prefixed (distinct from tt imdb keys)"
        );
        assert!(a
            .as_deref()
            .unwrap()
            .ends_with(&format!("_v{}", CACHE_VERSION)));
    }

    #[test]
    fn test_title_key_distinct_per_title_lang_intro() {
        let base = build_cache_key_for_title("A.Movie.2020", Some("eng"), false).unwrap();
        assert_ne!(
            base,
            build_cache_key_for_title("B.Movie.2020", Some("eng"), false).unwrap()
        );
        assert_ne!(
            base,
            build_cache_key_for_title("A.Movie.2020", Some("swe"), false).unwrap()
        );
        assert_ne!(
            base,
            build_cache_key_for_title("A.Movie.2020", Some("eng"), true).unwrap()
        );
    }

    #[test]
    fn test_title_key_empty_is_none_and_is_filesystem_safe() {
        assert!(build_cache_key_for_title("", Some("eng"), false).is_none());
        assert!(build_cache_key_for_title("   ", Some("eng"), false).is_none());
        // A path-traversal-y raw_name must NOT leak separators into the key.
        let k = build_cache_key_for_title("../../etc/passwd", None, false).unwrap();
        assert!(!k.contains('/') && !k.contains("..") && !k.contains(' '));
    }

    // --- resolve_cache_key: the shared fill/hit key resolver ---

    #[test]
    fn test_resolve_cache_key_disabled_when_cap_zero() {
        assert!(
            resolve_cache_key(
                0,
                Some("tt0119229"),
                None,
                None,
                Some("eng"),
                false,
                Some("X")
            )
            .is_none(),
            "cap 0 → no key (caching disabled), regardless of metadata"
        );
    }

    #[test]
    fn test_resolve_cache_key_prefers_imdb_then_falls_back_to_title() {
        // Search/torrent play (imdb present) → tt key.
        let k = resolve_cache_key(
            12288,
            Some("tt0119229"),
            Some(0),
            Some(0),
            Some("eng"),
            false,
            Some("Grosse.Pointe.Blank"),
        )
        .unwrap();
        assert!(
            k.starts_with("tt0119229_"),
            "imdb present → imdb key, got {k}"
        );
        // Library/bypass play (no imdb, has title) → lib key (5th facet).
        let k2 = resolve_cache_key(
            12288,
            None,
            None,
            None,
            Some("eng"),
            false,
            Some("Grosse.Pointe.Blank.1997.1080p"),
        )
        .unwrap();
        assert!(
            k2.starts_with("lib"),
            "no imdb + title → lib title-hash key, got {k2}"
        );
    }

    #[test]
    fn test_resolve_cache_key_none_when_no_imdb_and_no_title() {
        assert!(resolve_cache_key(12288, None, None, None, Some("eng"), false, None).is_none());
    }

    // --- cached_duration: server-side EXTINF sum for cache-hit casts ---

    #[test]
    fn test_cached_duration_sums_first_variant_extinf() {
        let tmp = TempDir::new().unwrap();
        let key = "lib00000000deadbeef_eng_nointro_v1";
        let dir = cache_dir_for_key(tmp.path(), key);
        fs::create_dir_all(&dir).unwrap();
        // Multi-variant master → first non-# line is the variant URI.
        fs::write(
            dir.join(MANIFEST_NAME),
            b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5640800\nstream_0.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1425600\nstream_1.m3u8\n",
        )
        .unwrap();
        fs::write(
            dir.join("stream_0.m3u8"),
            b"#EXTM3U\n#EXTINF:6.000,\nseg_0_00000.ts\n#EXTINF:6.000,\nseg_0_00001.ts\n#EXTINF:3.500,\nseg_0_00002.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(cached_duration(tmp.path(), key), Some(15.5));
    }

    #[test]
    fn test_cached_duration_none_when_missing_or_no_extinf() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            cached_duration(tmp.path(), "tt0_s00e00_eng_nointro_v1"),
            None
        );
        let key = "tt1_s00e00_eng_nointro_v1";
        let dir = cache_dir_for_key(tmp.path(), key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_NAME), b"#EXTM3U\nstream_0.m3u8\n").unwrap();
        fs::write(dir.join("stream_0.m3u8"), b"#EXTM3U\n#EXT-X-ENDLIST\n").unwrap();
        assert_eq!(cached_duration(tmp.path(), key), None);
    }

    // --- Path layout ---

    #[test]
    fn test_cache_dir_for_key_is_under_media_dir() {
        let dir = cache_dir_for_key(Path::new("/srv/media"), "tt1234567_s01e01_eng_intro_v1");
        assert_eq!(
            dir,
            PathBuf::from("/srv/media/hls_cache/tt1234567_s01e01_eng_intro_v1")
        );
    }

    #[test]
    fn test_cache_root_is_subdir_under_media_dir() {
        assert_eq!(
            cache_root(Path::new("/srv/media")),
            PathBuf::from("/srv/media/hls_cache")
        );
    }

    // --- Cache hit detection ---

    #[test]
    fn test_is_cache_hit_false_when_dir_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_cache_hit(tmp.path(), "tt1234567_s01e01_eng_intro_v1"));
    }

    #[test]
    fn test_is_cache_hit_false_when_only_manifest_present() {
        let tmp = TempDir::new().unwrap();
        let key = "tt1234567_s01e01_eng_intro_v1";
        let dir = cache_dir_for_key(tmp.path(), key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_NAME), b"#EXTM3U\n").unwrap();
        // Manifest exists but no complete-marker → partial fill, skip.
        assert!(!is_cache_hit(tmp.path(), key));
    }

    #[test]
    fn test_is_cache_hit_false_when_only_marker_present() {
        let tmp = TempDir::new().unwrap();
        let key = "tt1234567_s01e01_eng_intro_v1";
        let dir = cache_dir_for_key(tmp.path(), key);
        fs::create_dir_all(&dir).unwrap();
        mark_complete(&dir).unwrap();
        // Marker exists but no manifest → corruption / external delete.
        assert!(!is_cache_hit(tmp.path(), key));
    }

    #[test]
    fn test_is_cache_hit_true_with_marker_and_manifest() {
        let tmp = TempDir::new().unwrap();
        let key = "tt1234567_s01e01_eng_intro_v1";
        let dir = cache_dir_for_key(tmp.path(), key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_NAME), b"#EXTM3U\n").unwrap();
        mark_complete(&dir).unwrap();
        assert!(is_cache_hit(tmp.path(), key));
    }

    #[test]
    fn test_mark_complete_writes_v1_sentinel() {
        let tmp = TempDir::new().unwrap();
        mark_complete(tmp.path()).unwrap();
        let contents = fs::read_to_string(tmp.path().join(COMPLETE_MARKER)).unwrap();
        assert_eq!(contents, "v1\n");
    }

    // --- Size accounting ---

    #[test]
    fn test_cache_dir_size_bytes_empty_dir_returns_zero() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(cache_dir_size_bytes(tmp.path()), 0);
    }

    #[test]
    fn test_cache_dir_size_bytes_counts_files_in_subdirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("entry");
        fs::create_dir_all(&sub).unwrap();
        // Write known-size files. Use ≥4 KB so block accounting yields
        // something predictable on common filesystems (block size 512-4096).
        fs::write(sub.join("a.ts"), vec![0u8; 8192]).unwrap();
        fs::write(sub.join("b.ts"), vec![0u8; 8192]).unwrap();
        let size = cache_dir_size_bytes(tmp.path());
        // Each 8 KB file uses ~16 blocks of 512 bytes = 8192 bytes (typical),
        // but minimum block allocation may bump this. Lower-bound is 16384.
        assert!(
            size >= 16_384,
            "expected ≥16384 bytes (2× 8KB files), got {}",
            size
        );
    }

    #[test]
    fn test_cache_dir_size_bytes_handles_missing_dir() {
        // Lazy: missing dir is treated as 0 bytes. Don't panic.
        assert_eq!(
            cache_dir_size_bytes(Path::new("/nonexistent/path/spela")),
            0
        );
    }

    // --- LRU age ordering ---

    #[test]
    fn test_cache_entries_by_age_orders_oldest_first() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("entry_a");
        let b = tmp.path().join("entry_b");
        let c = tmp.path().join("entry_c");
        fs::create_dir_all(&a).unwrap();
        sleep(Duration::from_millis(20));
        fs::create_dir_all(&b).unwrap();
        sleep(Duration::from_millis(20));
        fs::create_dir_all(&c).unwrap();
        let entries = cache_entries_by_age(tmp.path());
        let names: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["entry_a", "entry_b", "entry_c"]);
    }

    #[test]
    fn test_cache_entries_by_age_handles_empty_root() {
        let tmp = TempDir::new().unwrap();
        assert!(cache_entries_by_age(tmp.path()).is_empty());
    }

    // --- LRU eviction ---

    #[test]
    fn test_prune_cache_to_fit_no_op_when_under_cap() {
        let tmp = TempDir::new().unwrap();
        let entry = tmp.path().join("entry_a");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("seg.ts"), vec![0u8; 4096]).unwrap();
        // Cap of 100 MB → no eviction needed.
        let evicted = prune_cache_to_fit(tmp.path(), 100 * 1024 * 1024);
        assert_eq!(evicted, 0);
        assert!(entry.exists());
    }

    #[test]
    fn test_prune_cache_to_fit_evicts_oldest_first() {
        let tmp = TempDir::new().unwrap();
        // Each entry has a ~32 KB file. Setting a small cap forces eviction.
        let a = tmp.path().join("entry_a");
        let b = tmp.path().join("entry_b");
        let c = tmp.path().join("entry_c");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("seg.ts"), vec![0u8; 32 * 1024]).unwrap();
        sleep(Duration::from_millis(20));
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("seg.ts"), vec![0u8; 32 * 1024]).unwrap();
        sleep(Duration::from_millis(20));
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("seg.ts"), vec![0u8; 32 * 1024]).unwrap();

        // Cap allows ~one entry. Eviction targets oldest (entry_a) first.
        let evicted = prune_cache_to_fit(tmp.path(), 40 * 1024);
        assert!(evicted >= 1, "should have evicted ≥1 entry");
        assert!(!a.exists(), "oldest entry_a should have been deleted");
        // Newest entry_c always preserved.
        assert!(c.exists(), "newest entry_c should be preserved");
    }

    #[test]
    fn test_prune_cache_to_fit_does_not_delete_root() {
        let tmp = TempDir::new().unwrap();
        let entry = tmp.path().join("entry_a");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("seg.ts"), vec![0u8; 4096]).unwrap();
        // Force evict-everything by cap of 0.
        prune_cache_to_fit(tmp.path(), 0);
        // Root must still exist.
        assert!(tmp.path().exists());
    }

    // --- Round-trip: build key → mark complete → hit ---

    #[test]
    fn test_full_roundtrip_build_mark_hit() {
        let tmp = TempDir::new().unwrap();
        let key = build_cache_key(Some("tt1399664"), Some(2), Some(4), Some("eng"), true).unwrap();
        assert_eq!(key, "tt1399664_s02e04_eng_intro_v1");
        let dir = cache_dir_for_key(tmp.path(), &key);
        // Pre-fill: no hit.
        assert!(!is_cache_hit(tmp.path(), &key));
        // Build the cache entry as the reaper would: create dir, write
        // manifest, mark complete.
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_NAME), b"#EXTM3U\n").unwrap();
        mark_complete(&dir).unwrap();
        // Post-fill: hit.
        assert!(is_cache_hit(tmp.path(), &key));
    }

    #[test]
    fn test_constants_are_stable() {
        // Pin so a future refactor doesn't silently change the on-disk
        // cache schema. Bumping CACHE_VERSION is intentional; this catches
        // accidental changes.
        assert_eq!(CACHE_VERSION, 1);
        assert_eq!(CACHE_DIR_NAME, "hls_cache");
        assert_eq!(COMPLETE_MARKER, ".complete.v1");
        assert_eq!(MANIFEST_NAME, "master.m3u8");
    }
}
