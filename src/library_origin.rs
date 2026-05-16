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
use serde::Deserialize;
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
    let state: Shared = std::sync::Arc::new(LibraryOriginState {
        roots,
        handles: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/library/match", get(handle_match))
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
}
