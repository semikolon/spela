//! v3.6.0 Local Library Streaming — **library-host origin**.
//!
//! `spela serve-library` runs this. It is a *dumb matcher + Range file
//! server* for a host that holds media the spela server (a DIFFERENT LAN
//! host) cannot see on its own filesystem. It does NO transcoding and NO
//! casting — the spela server stays the sole transcode/cast authority and
//! consumes this origin purely as an `http://` ffmpeg input.
//!
//! Routes:
//!   * `GET /library/match?title=&quality=&year=`  → reuse the exact
//!     `server::first_local_bypass_match` matcher over this host's
//!     `library_dirs`; on a hit, mint a short-TTL opaque handle and return
//!     `{handle,size,container}`; on miss `404`.
//!   * `GET /library/stream?h=<handle>`            → Range-capable raw-file
//!     serve of the path the handle resolves to.
//!
//! Security model (full threat table: docs/LOCAL_LIBRARY_STREAMING_PLAN.md
//! §9). The single highest-risk surface in the feature, so the boundary is
//! deliberately layered and **the client never supplies a path**:
//!   1. No raw `?path=` — only an opaque, server-issued, expiring handle
//!      that maps ONLY to a path the matcher itself returned. Path
//!      traversal is eliminated by construction.
//!   2. Defense in depth: the resolved path is `canonicalize()`d and
//!      asserted to be a prefix-child of a configured (also canonicalized)
//!      root, AFTER which a symlink check refuses links (mirrors
//!      `server::local_bypass_file_is_healthy`).
//!   3. The spela server only ever builds the origin URL from an operator-
//!      configured `remote_origins` entry + a handle it just received from
//!      that same origin (no SSRF pivot).
//!
//! Like the main server (see server.rs CORS rationale ~150), this origin
//! has no browser UI, no cookies, no auth, no per-user state — the path
//! boundary above is the real defense, not Host gating.

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::server::first_local_bypass_match;
use crate::torrent_stream::parse_range_header;

/// How long a minted `/library/match` handle stays valid. Covers the
/// server's `detect_codecs` ffprobe + `transcode_hls` start latency with
/// generous headroom; a re-resolved play simply mints a fresh handle.
const HANDLE_TTL: Duration = Duration::from_secs(120);

struct LibraryOriginState {
    /// Canonicalized configured library roots. A served path must
    /// canonicalize to a prefix-child of one of these.
    roots: Vec<PathBuf>,
    /// opaque handle → (canonical absolute path, issued_at).
    handles: Mutex<HashMap<String, (PathBuf, Instant)>>,
}

type Shared = std::sync::Arc<LibraryOriginState>;

#[derive(Deserialize)]
struct MatchParams {
    title: String,
    quality: Option<String>,
    /// Accepted for forward-compat / explicitness; year filtering is
    /// already encoded in `title` by the matcher, so this is advisory.
    #[allow(dead_code)]
    year: Option<String>,
}

#[derive(Deserialize)]
struct StreamParams {
    h: String,
}

// ---------------------------------------------------------------------------
// v3.7 web-remote — `GET /library/list` (My Library browse, spec T-2).
//
// Sibling of `/library/match`: same configured roots, same no-path-leakage
// posture (only display/identity metadata crosses the wire), but instead of
// resolving ONE title it enumerates EVERY browsable entry so the web-remote
// can render the curated BOHR collection as a poster grid and tap-play any
// item back through the existing `do_play` → `/library/match` bridge.
// ---------------------------------------------------------------------------

/// Container extensions treated as a playable library entry.
const LIBRARY_VIDEO_EXTS: &[&str] = &["mkv", "mp4", "m4v", "avi"];

/// Sanity floor: a real feature is ≥100 MB. Smaller entries are
/// samples / extras / stubs — excluded so every tile the SPA shows is
/// actually tap-playable. Mirrors `server::top_level_file_is_healthy`'s
/// `MIN_MOVIE_SIZE_BYTES`. (Logical size only — sparseness is a torrent
/// artifact; curated library files served here are genuinely full.)
const MIN_LIBRARY_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// One browsable curated-library item. Returned by `serve-library`'s
/// `GET /library/list` and re-emitted (merged + best-effort poster-
/// enriched by spela's `GET /library` aggregator). `raw_name` is the
/// EXACT top-level entry name (folder for directory releases, filename
/// for single-file releases) — it is what the SPA echoes back as the
/// `/play` title so `do_play`'s `title_tokens_match` re-resolves THIS
/// entry through the existing bridge (web-remote AC-3.3). Only
/// display/identity metadata crosses the wire — never an absolute path
/// (same posture as `/library/match`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    pub raw_name: String,
    pub size_bytes: u64,
    pub container: String,
    /// Best-effort TMDB art, filled by spela's aggregator (web-remote
    /// T-4). `serve-library` always emits `None` — it is a dumb file
    /// server with no TMDB key; spela's `/library` enriches, and the
    /// frontend renders a clean titled fallback tile on `None` (AC-3.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
}

/// Lowercase-preserving separator normalisation: `.`/`_`/`(`/`)`/`[`/`]`
/// → space, collapse runs, trim. Original case kept (display).
fn clean_release_tokens(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '.' | '_' | '(' | ')' | '[' | ']' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a release folder/file name into a clean display title + year.
/// Pure — no I/O, RED-GREEN tested.
///
/// Ordered strategy (robust for curated `Title (YYYY)` AND scene
/// `Title.YYYY.1080p.GROUP`):
///   1. A parenthesised/bracketed `(YYYY)`/`[YYYY]` wins (the curated
///      BOHR convention) — title is everything before it. This also
///      disambiguates a numeric title from its release year
///      (`2012 (2009)` → title "2012", year 2009).
///   2. Else the first bare 4-digit token in 1900..=2099 is the year;
///      title is the tokens before it (scene convention).
///   3. Else no year; the whole cleaned string is the title.
///
/// `2160`/`1080`/`720` never false-match (outside 1900..=2099). Known
/// best-effort limit (acceptable per spec R-2 — `raw_name`, not this
/// parsed title, is the play key): a scene name with a year IN the
/// title and no parens (`Blade.Runner.2049.2017.1080p`) mis-splits;
/// the curated BOHR `Blade Runner 2049 (2017)` form parses correctly.
pub fn parse_library_name(raw: &str) -> (String, Option<u32>) {
    // Strip a trailing video extension (single-file entries pass the
    // filename; directory entries have none — harmless either way).
    let stem = match raw.rsplit_once('.') {
        Some((base, ext)) if LIBRARY_VIDEO_EXTS.contains(&ext.to_lowercase().as_str()) => base,
        _ => raw,
    };
    let in_range = |n: u32| (1900..=2099).contains(&n);

    // 1. Parenthesised/bracketed year: `( D D D D )` (ASCII; byte
    //    indices are char boundaries because `(`/digits are 1-byte).
    let bytes = stem.as_bytes();
    for i in 0..bytes.len() {
        let open = bytes[i];
        if open != b'(' && open != b'[' {
            continue;
        }
        let close = if open == b'(' { b')' } else { b']' };
        if let (Some(&a), Some(&b), Some(&c), Some(&d), Some(&e)) = (
            bytes.get(i + 1),
            bytes.get(i + 2),
            bytes.get(i + 3),
            bytes.get(i + 4),
            bytes.get(i + 5),
        ) {
            if a.is_ascii_digit()
                && b.is_ascii_digit()
                && c.is_ascii_digit()
                && d.is_ascii_digit()
                && e == close
            {
                if let Ok(y) = stem[i + 1..i + 5].parse::<u32>() {
                    if in_range(y) {
                        let title = clean_release_tokens(&stem[..i]);
                        if !title.is_empty() {
                            return (title, Some(y));
                        }
                    }
                }
            }
        }
    }

    // 2. First bare in-range 4-digit token.
    let cleaned = clean_release_tokens(stem);
    let toks: Vec<&str> = cleaned.split(' ').filter(|t| !t.is_empty()).collect();
    for (idx, t) in toks.iter().enumerate() {
        if t.len() == 4 && t.bytes().all(|c| c.is_ascii_digit()) {
            if let Ok(y) = t.parse::<u32>() {
                if in_range(y) {
                    let title = toks[..idx].join(" ");
                    if !title.is_empty() {
                        return (title, Some(y));
                    }
                }
            }
        }
    }

    // 3. No usable year.
    (cleaned, None)
}

/// Largest non-`transcoded` playable file directly inside `dir`, as
/// `(size_bytes, ext)`. `None` if the directory holds no feature file.
/// The largest (not first) wins so a `sample.mkv` never represents the
/// directory.
fn largest_inner_media(dir: &Path) -> Option<(u64, String)> {
    let mut best: Option<(u64, String)> = None;
    for sub in std::fs::read_dir(dir).ok()?.flatten() {
        let p = sub.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !LIBRARY_VIDEO_EXTS.contains(&ext.as_str()) || fname.starts_with("transcoded") {
            continue;
        }
        let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if best.as_ref().map(|(b, _)| sz > *b).unwrap_or(true) {
            best = Some((sz, ext));
        }
    }
    best
}

/// Enumerate browsable media under each root (web-remote T-2). Mirrors
/// the top-level-entry structure of `server::find_local_bypass_match`:
/// a top-level `.mkv`/`.mp4` IS an entry; a directory is represented by
/// its LARGEST inner playable file (the feature, not a sample).
/// `raw_name` is always the TOP-LEVEL entry name so it round-trips
/// through `do_play`'s matcher (AC-3.3). Sub-floor entries are skipped.
/// No absolute path ever leaves this function. Pure filesystem —
/// tempfile-testable.
pub fn enumerate_library(roots: &[PathBuf]) -> Vec<LibraryEntry> {
    let mut out: Vec<LibraryEntry> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let raw_name = entry.file_name().to_string_lossy().to_string();

            let (size_bytes, container) = if ft.is_file() {
                let p = entry.path();
                let ext = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if !LIBRARY_VIDEO_EXTS.contains(&ext.as_str()) || fname.starts_with("transcoded") {
                    continue;
                }
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                (sz, ext)
            } else if ft.is_dir() {
                match largest_inner_media(&entry.path()) {
                    Some(v) => v,
                    None => continue,
                }
            } else {
                continue;
            };

            if size_bytes < MIN_LIBRARY_FILE_BYTES {
                continue;
            }
            let (title, year) = parse_library_name(&raw_name);
            out.push(LibraryEntry {
                title,
                year,
                raw_name,
                size_bytes,
                container,
                poster_url: None,
            });
        }
    }
    // Stable alphabetical order — a calm grid is part of NFR-1.
    out.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.year.cmp(&b.year))
    });
    out
}

/// Entry point for `spela serve-library`.
pub async fn run(config: Config, port_override: Option<u16>) -> Result<()> {
    let roots = canonicalize_roots(&config.library_dirs());
    if roots.is_empty() {
        anyhow::bail!(
            "serve-library: no usable library_dirs in config (set `library_dirs` \
             in ~/.config/spela/config.toml to existing directories)"
        );
    }
    for r in &roots {
        tracing::info!("serve-library root: {:?}", r);
    }

    let port = port_override.unwrap_or(config.library_serve_port);

    // v3.6.3 self-warm. A remote library typically lives on a USB HDD that
    // macOS spins down after ~10 min idle. A cold first `/library/match`
    // then blocks on spin-up (5-15s+), which used to trip the caller's
    // timeout into a silent torrent fallback (wrong source/quality). Keep
    // the backing drive spun up: touch each root immediately, then every
    // 180s (well under the macOS disk-idle default). Cheap (one dir entry);
    // a dedicated std thread so a cold spin-up never stalls the async
    // runtime. This is HALF the "bridge works flawlessly" guarantee — the
    // do_play liveness-gated generous timeout is the other half. Do NOT
    // remove either half.
    let warm_roots: Vec<PathBuf> = roots.clone();
    std::thread::Builder::new()
        .name("serve-library-warm".into())
        .spawn(move || loop {
            for r in &warm_roots {
                let _ = std::fs::read_dir(r).map(|mut it| it.next());
            }
            std::thread::sleep(Duration::from_secs(180));
        })
        .ok();

    let state: Shared = std::sync::Arc::new(LibraryOriginState {
        roots,
        handles: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/library/match", get(handle_match))
        .route("/library/list", get(handle_library_list))
        .route("/library/stream", get(handle_stream))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("serve-library: failed to bind {}", addr))?;
    tracing::info!("spela serve-library listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Canonicalize each configured root, dropping (with a warning) any that
/// don't resolve. Canonical roots are required so the per-request
/// prefix-check in `resolve_under_roots` is sound against `..`/symlinks.
fn canonicalize_roots(raw: &[PathBuf]) -> Vec<PathBuf> {
    raw.iter()
        .filter_map(|p| match std::fs::canonicalize(p) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("serve-library: skipping unusable root {:?}: {}", p, e);
                None
            }
        })
        .collect()
}

/// Pure security boundary: a candidate path is acceptable IFF, after
/// `canonicalize()` (which resolves `..` and symlinks in parent
/// components), it is `root` itself or a descendant of one canonical
/// `root`, AND it is not itself a symlink. Returns the canonical path.
///
/// Pure + tempfile-testable. This is THE path-escape guard — RED-GREEN
/// tested for `..` escape and symlink escape.
fn resolve_under_roots(roots: &[PathBuf], candidate: &Path) -> Option<PathBuf> {
    // Reject symlinks before canonicalize (symlink_metadata does not
    // follow). Mirrors server::local_bypass_file_is_healthy.
    match std::fs::symlink_metadata(candidate) {
        Ok(m) if m.file_type().is_symlink() => {
            tracing::warn!("serve-library: refusing symlink {:?}", candidate);
            return None;
        }
        Ok(_) => {}
        Err(_) => return None,
    }
    let canon = std::fs::canonicalize(candidate).ok()?;
    for root in roots {
        if canon == *root || canon.starts_with(root) {
            return Some(canon);
        }
    }
    tracing::warn!(
        "serve-library: path {:?} escapes all configured roots",
        candidate
    );
    None
}

fn mint_handle(path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    n.hash(&mut hasher);
    nanos.hash(&mut hasher);
    format!("{:016x}{:016x}", hasher.finish(), nanos as u64 ^ n)
}

fn prune_expired(map: &mut HashMap<String, (PathBuf, Instant)>) {
    let now = Instant::now();
    map.retain(|_, (_, issued)| now.duration_since(*issued) < HANDLE_TTL);
}

async fn handle_match(State(state): State<Shared>, Query(p): Query<MatchParams>) -> Response {
    if p.title.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "title required").into_response();
    }
    let empty = std::collections::HashSet::new();
    // Reuse the EXACT server-side matcher + multi-root helper. expected
    // bytes = 0 (the origin host has no torrent-size expectation; the
    // title/year/quality + health matrix is the discriminator).
    let hit = first_local_bypass_match(&state.roots, &p.title, p.quality.as_deref(), 0, &empty);
    let Some(path) = hit else {
        return (StatusCode::NOT_FOUND, "no local match").into_response();
    };
    let Some(canon) = resolve_under_roots(&state.roots, &path) else {
        // Matched something that fails the security boundary — treat as miss.
        return (StatusCode::NOT_FOUND, "no servable match").into_response();
    };
    let size = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
    let container = canon
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let handle = mint_handle(&canon);
    {
        let mut map = state.handles.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut map);
        map.insert(handle.clone(), (canon.clone(), Instant::now()));
    }
    tracing::info!(
        "serve-library: /match '{}' -> {:?} ({} bytes)",
        p.title,
        canon,
        size
    );
    Json(json!({ "handle": handle, "size": size, "container": container })).into_response()
}

/// `GET /library/list` — enumerate this host's configured roots into
/// `[LibraryEntry]` (web-remote T-2). Same no-path-leakage posture as
/// `/library/match`. The directory walk (potentially heavy on a cold
/// USB HDD) runs on a blocking thread so the async runtime is never
/// stalled; the caller (spela `/library`) gates it behind a liveness
/// ping + generous timeout (web-remote T-3 / the v3.6.3 pattern).
async fn handle_library_list(State(state): State<Shared>) -> Response {
    let roots = state.roots.clone();
    match tokio::task::spawn_blocking(move || enumerate_library(&roots)).await {
        Ok(entries) => {
            tracing::info!("serve-library: /library/list -> {} entries", entries.len());
            Json(json!({ "entries": entries })).into_response()
        }
        Err(e) => {
            tracing::warn!("serve-library: /library/list join error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "enumeration failed").into_response()
        }
    }
}

async fn handle_stream(
    State(state): State<Shared>,
    Query(p): Query<StreamParams>,
    headers: HeaderMap,
) -> Response {
    // Resolve handle → path. Unknown/expired => 410 GONE (distinct from
    // 404 so the server can tell "never matched" from "matched, expired").
    let path = {
        let mut map = state.handles.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut map);
        match map.get(&p.h) {
            Some((path, _)) => path.clone(),
            None => return (StatusCode::GONE, "unknown or expired handle").into_response(),
        }
    };
    // Defense in depth: re-validate the path is still under a root and not
    // a symlink, every request (TOCTOU + handle-map integrity).
    let Some(canon) = resolve_under_roots(&state.roots, &path) else {
        return (StatusCode::FORBIDDEN, "path failed security boundary").into_response();
    };

    let total = match std::fs::metadata(&canon) {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::warn!("serve-library: stat {:?} failed: {}", canon, e);
            return (StatusCode::NOT_FOUND, "file unavailable").into_response();
        }
    };
    let range = match parse_range_header(headers.get(header::RANGE), total) {
        Ok(r) => r,
        Err(e) => return e.http_status().into_response(),
    };

    let mut file = match tokio::fs::File::open(&canon).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("serve-library: open {:?} failed: {}", canon, e);
            return (StatusCode::NOT_FOUND, "file unavailable").into_response();
        }
    };
    if file
        .seek(std::io::SeekFrom::Start(range.start))
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let len = range.len();
    let partial = range.start != 0 || len != total;
    let body = Body::from_stream(ReaderStream::new(file.take(len)));

    let mut resp = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::CONTENT_TYPE, "application/octet-stream");
    let status = if partial {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", range.start, range.start + len - 1, total),
        );
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    resp.status(status)
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dense_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![7u8; 256]).unwrap();
        p
    }

    #[test]
    fn resolve_under_roots_accepts_file_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let canon_root = std::fs::canonicalize(root.path()).unwrap();
        let f = dense_file(&canon_root, "Her.2013.1080p.mkv");
        let got = resolve_under_roots(std::slice::from_ref(&canon_root), &f);
        assert_eq!(got, Some(std::fs::canonicalize(&f).unwrap()));
    }

    #[test]
    fn resolve_under_roots_rejects_parent_escape() {
        // `<root>/../secret` must NOT resolve under root.
        let root = tempfile::tempdir().unwrap();
        let canon_root = std::fs::canonicalize(root.path()).unwrap();
        let outside = canon_root.parent().unwrap().join("secret_outside.bin");
        std::fs::write(&outside, b"top secret").unwrap();
        let escape = canon_root.join("..").join("secret_outside.bin");
        assert!(
            resolve_under_roots(std::slice::from_ref(&canon_root), &escape).is_none(),
            "../ escape from configured root MUST be refused"
        );
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn resolve_under_roots_rejects_symlink_escape() {
        // A symlink INSIDE the root pointing OUTSIDE must be refused.
        let root = tempfile::tempdir().unwrap();
        let canon_root = std::fs::canonicalize(root.path()).unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let secret = secret_dir.path().join("passwd");
        std::fs::write(&secret, b"root:x:0:0").unwrap();
        let link = canon_root.join("escape.mkv");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        #[cfg(unix)]
        assert!(
            resolve_under_roots(std::slice::from_ref(&canon_root), &link).is_none(),
            "symlink escaping the root MUST be refused"
        );
    }

    #[test]
    fn mint_handle_is_unique_per_call() {
        let p = PathBuf::from("/x/y/Her.2013.mkv");
        let a = mint_handle(&p);
        let b = mint_handle(&p);
        assert_ne!(a, b, "handles must not collide for repeat calls");
        assert!(a.len() >= 16);
    }

    #[test]
    fn prune_expired_drops_only_old_entries() {
        let mut m: HashMap<String, (PathBuf, Instant)> = HashMap::new();
        m.insert("fresh".into(), (PathBuf::from("/a"), Instant::now()));
        m.insert(
            "stale".into(),
            (
                PathBuf::from("/b"),
                Instant::now() - HANDLE_TTL - Duration::from_secs(1),
            ),
        );
        prune_expired(&mut m);
        assert!(m.contains_key("fresh"));
        assert!(!m.contains_key("stale"));
    }

    // ----- web-remote T-2: parse_library_name + enumerate_library -----

    /// Logical-size-only sparse file (instant, zero disk) — exercises the
    /// 100 MB floor without writing real data. `enumerate_library`
    /// deliberately checks logical size only (sparseness is a torrent
    /// artifact; curated library files are genuinely full).
    fn sparse_file(path: &Path, len: u64) {
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
    }

    #[test]
    fn parse_library_name_curated_paren_convention() {
        assert_eq!(
            parse_library_name("Grosse Pointe Blank (1997)"),
            ("Grosse Pointe Blank".to_string(), Some(1997))
        );
        assert_eq!(
            parse_library_name("Her (2013).mkv"),
            ("Her".to_string(), Some(2013))
        );
        // Numeric title disambiguated by the parenthesised release year.
        assert_eq!(
            parse_library_name("2012 (2009)"),
            ("2012".to_string(), Some(2009))
        );
        // Year-in-title curated form parses correctly (paren wins).
        assert_eq!(
            parse_library_name("Blade Runner 2049 (2017)"),
            ("Blade Runner 2049".to_string(), Some(2017))
        );
    }

    #[test]
    fn parse_library_name_scene_convention() {
        assert_eq!(
            parse_library_name("Inception.2010.1080p.BluRay.x264-GROUP"),
            ("Inception".to_string(), Some(2010))
        );
        assert_eq!(
            parse_library_name("The.Matrix.1999.REMASTERED.2160p"),
            ("The Matrix".to_string(), Some(1999))
        );
    }

    #[test]
    fn parse_library_name_no_year_and_resolution_not_mistaken() {
        assert_eq!(
            parse_library_name("Some Indie Film"),
            ("Some Indie Film".to_string(), None)
        );
        // 2160 / 1080 are NOT years (outside 1900..=2099) — must not split.
        assert_eq!(
            parse_library_name("Avatar.2160p.UHD"),
            ("Avatar 2160p UHD".to_string(), None)
        );
        // Bare numeric-title folder, no release year → no false year.
        assert_eq!(parse_library_name("2012"), ("2012".to_string(), None));
    }

    #[test]
    fn enumerate_library_structure_and_floor() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path();
        // Top-level single-file release (curated paren convention).
        sparse_file(&rp.join("Her (2013).mkv"), 150 * 1024 * 1024);
        // Directory release: LARGEST inner file represents it; the
        // sample.mkv must NOT win.
        let dir = rp.join("Inception (2010)");
        std::fs::create_dir(&dir).unwrap();
        sparse_file(&dir.join("Inception.1080p.BluRay.mkv"), 200 * 1024 * 1024);
        sparse_file(&dir.join("sample.mkv"), 5 * 1024 * 1024);
        // Excluded: below floor, non-video, transcoded artifact.
        sparse_file(&rp.join("tiny.mp4"), 1024 * 1024);
        std::fs::write(rp.join("readme.txt"), b"notes").unwrap();
        sparse_file(&rp.join("transcoded_old.mkv"), 150 * 1024 * 1024);

        let roots = vec![rp.to_path_buf()];
        let got = enumerate_library(&roots);

        assert_eq!(got.len(), 2, "got: {:?}", got);

        // Alphabetical (Her < Inception).
        assert_eq!(got[0].title, "Her");
        assert_eq!(got[0].year, Some(2013));
        assert_eq!(got[0].raw_name, "Her (2013).mkv"); // matcher round-trip key
        assert_eq!(got[0].container, "mkv");
        assert!(got[0].size_bytes >= 100 * 1024 * 1024);
        assert!(got[0].poster_url.is_none());

        assert_eq!(got[1].title, "Inception");
        assert_eq!(got[1].year, Some(2010));
        // raw_name is the FOLDER name (do_play matcher iterates top-level
        // entries) — NOT the inner filename.
        assert_eq!(got[1].raw_name, "Inception (2010)");
        // Largest inner file represents the dir, not the 5 MB sample.
        assert_eq!(got[1].size_bytes, 200 * 1024 * 1024);
    }
}
