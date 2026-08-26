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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::sleep;
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::server::first_local_bypass_match;
use crate::torrent_stream::parse_range_header;

/// How long a minted `/library/match` handle stays valid AFTER ITS LAST ACCESS
/// (sliding — `handle_stream` refreshes the timestamp on every request). An
/// actively-watched or -seeked file therefore never expires; the TTL is really an
/// IDLE timeout that only prunes an abandoned handle (VLC closed). It must still
/// span the worst case of a single uninterrupted connection — a full movie played
/// straight through with no new request, then a seek — so it's watch-length, not the
/// old 2 min (which returned 410 GONE to VLC's seek after ~2 min → "input can't be
/// opened", the direct-VLC seek bug, 2026-08-05). 6 h covers any movie + a long pause.
const HANDLE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// v3.8.0 drive-aware state machine. Replaces the v3.6.0-pre-3.8 model
/// where serve-library exited 1 the moment no roots canonicalized (which
/// led launchd's KeepAlive into a 10s crash-loop that drowned the logs
/// in identical "no usable library_dirs" lines until someone noticed —
/// the exact failure-as-noise-not-signal mode the global directives
/// forbid). Now the daemon stays alive in `Waiting`, periodically re-
/// probes, logs ONLY on transitions, and fires a single ntfy when a
/// drive vanishes or comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibraryStateKind {
    /// Before the first probe completes. Distinct from `Serving` so the
    /// initial-state log fires exactly once even if startup happens to
    /// be Serving.
    Initial,
    /// All configured roots canonicalize cleanly.
    Serving,
    /// Some configured roots are reachable, others aren't.
    Degraded,
    /// None of the configured roots canonicalize — drives missing.
    /// `/library/{list,match}` return 503 + JSON until at least one
    /// root recovers.
    Waiting,
}

/// Pure: classify operational state from healthy vs configured root
/// counts. Precondition (enforced upstream in `run`): `configured > 0`
/// — an empty `library_dirs` is a genuine config error and the daemon
/// bails before reaching the state machine. The `configured == 0` arm
/// here is defensive only.
fn classify_state(healthy: usize, configured: usize) -> LibraryStateKind {
    if configured == 0 {
        return LibraryStateKind::Waiting;
    }
    if healthy == 0 {
        LibraryStateKind::Waiting
    } else if healthy < configured {
        LibraryStateKind::Degraded
    } else {
        LibraryStateKind::Serving
    }
}

/// Quiet sibling of `canonicalize_roots` (the pre-3.8 version that
/// warned per failing path on every call). The state machine's probe
/// runs every 30 s and re-canonicalizes — logging at WARN per missing
/// root each probe would re-introduce the spam this whole refactor
/// exists to eliminate. The transition logger handles the user-visible
/// signal instead.
fn canonicalize_roots_quiet(raw: &[PathBuf]) -> Vec<PathBuf> {
    raw.iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

struct LibraryOriginState {
    /// Configured library_dirs from config — RAW (not canonicalized).
    /// Re-canonicalized every probe to detect drive appearance/loss.
    /// Immutable for daemon lifetime (restart to pick up config edits).
    configured_roots: Vec<PathBuf>,
    /// Currently-usable canonicalized subset (re-probed every 30 s).
    /// Handlers read THIS, never `configured_roots`, so a vanished
    /// drive immediately disappears from `/library/list` results.
    healthy_roots: Mutex<Vec<PathBuf>>,
    /// Last observed state. Transition detection compares the result of
    /// the next probe against this, then overwrites. `Initial → ...`
    /// is fabricated on first probe so the startup log always fires.
    last_state: Mutex<LibraryStateKind>,
    /// opaque handle → (canonical absolute path, issued_at).
    handles: Mutex<HashMap<String, (PathBuf, Instant)>>,
    /// ntfy base URL for transition alerts. Empty string disables.
    ntfy_url: String,
}

type Shared = Arc<LibraryOriginState>;

#[derive(Deserialize)]
struct MatchParams {
    title: String,
    quality: Option<String>,
    /// Exact inner filename to serve, chosen by the pack chooser. When set the
    /// title-matcher is BYPASSED: `title` names the top-level directory and this
    /// names the file within it, so a multi-episode pack plays the episode the
    /// user actually picked instead of whatever the matcher lands on. Still
    /// funnelled through `resolve_under_roots`, so a traversal attempt in this
    /// value is refused like any other. 2026-08-26.
    file: Option<String>,
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
    /// Playable media files inside a DIRECTORY release. Omitted for single-file
    /// releases and for directories holding exactly one file, so `>1` is the
    /// SPA's "this card is a pack, open a chooser" signal. Without it a pack
    /// renders as one title with one poster and silently plays whichever file
    /// the matcher happens to land on (2026-08-26).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u32>,
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

/// Every playable media file directly inside `dir`, name-sorted so an episode
/// pack lists in episode order. Same extension + `transcoded` filters as
/// `largest_inner_media`, which it complements: that one answers "how big is
/// this release", this one answers "what is actually IN it". 2026-08-26.
fn list_inner_media(dir: &Path) -> Vec<(String, u64, String)> {
    let mut out: Vec<(String, u64, String)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for sub in rd.flatten() {
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
        if sz < MIN_LIBRARY_FILE_BYTES {
            continue;
        }
        out.push((fname.to_string(), sz, ext));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
            let file_count = if ft.is_dir() {
                let n = list_inner_media(&entry.path()).len();
                (n > 1).then_some(n as u32)
            } else {
                None
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
                file_count,
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
    let configured = config.library_dirs();
    if configured.is_empty() {
        // Genuine CONFIG error (distinct from "drive unplugged" which the
        // state machine handles gracefully below). Bail loudly — the user
        // needs to edit config.toml; we have nothing to serve in any state.
        anyhow::bail!(
            "serve-library: library_dirs is empty in config (set `library_dirs` \
             in ~/.config/spela/config.toml to existing directories)"
        );
    }

    let healthy_initial = canonicalize_roots_quiet(&configured);
    let state_initial = classify_state(healthy_initial.len(), configured.len());
    log_initial_state(state_initial, &healthy_initial, &configured);

    let port = port_override.unwrap_or(config.library_serve_port);

    let state: Shared = Arc::new(LibraryOriginState {
        configured_roots: configured,
        healthy_roots: Mutex::new(healthy_initial),
        last_state: Mutex::new(LibraryStateKind::Initial),
        handles: Mutex::new(HashMap::new()),
        ntfy_url: config.library_ntfy_url.clone(),
    });

    // Seed the transition detector by overwriting Initial → state_initial
    // through the probe path so the startup log already fired via
    // log_initial_state above, but the next *real* transition is detected
    // against state_initial (not Initial). The state machine probe handles
    // this on its first tick.
    {
        let mut last = state.last_state.lock().unwrap_or_else(|e| e.into_inner());
        *last = state_initial;
    }

    // v3.8.0 state-machine probe. Replaces v3.6.3's std-thread self-warm
    // (the warming job is now subsumed: every 30 s probe touches each
    // healthy root, which is well under macOS's ~10 min USB-HDD idle-
    // spin-down threshold). FS I/O runs inside `spawn_blocking` so a cold
    // spin-up never stalls the axum runtime. Logs ONLY on transitions —
    // steady state is silent (the v3.7-era log-spam-on-missing-drive is
    // structurally gone).
    let probe_state = state.clone();
    tokio::spawn(async move { run_state_machine_probe(probe_state).await });

    let app = Router::new()
        .route("/library/match", get(handle_match))
        .route("/library/list", get(handle_library_list))
        .route("/library/files", get(handle_library_files))
        .route("/library/stream", get(handle_stream))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("serve-library: failed to bind {}", addr))?;
    tracing::info!("spela serve-library listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

/// One-shot startup log line. Severity matches user-actionability:
/// Serving = INFO, Degraded/Waiting = WARN. Steady state after this is
/// silent until `log_transition` fires.
fn log_initial_state(state: LibraryStateKind, healthy: &[PathBuf], configured: &[PathBuf]) {
    match state {
        LibraryStateKind::Serving => {
            tracing::info!(
                "serve-library: SERVING — {} root(s): {:?}",
                healthy.len(),
                healthy
            );
        }
        LibraryStateKind::Degraded => {
            tracing::warn!(
                "serve-library: DEGRADED — {}/{} configured roots healthy ({} missing); \
                 daemon serving the healthy subset, ntfy alert dispatched if configured",
                healthy.len(),
                configured.len(),
                configured.len() - healthy.len()
            );
        }
        LibraryStateKind::Waiting => {
            tracing::warn!(
                "serve-library: WAITING — 0/{} configured roots healthy. \
                 HTTP /library/{{list,match}} returns 503 until drives mount. \
                 Probe re-checks every 30s; ntfy alert on recovery.",
                configured.len()
            );
        }
        LibraryStateKind::Initial => unreachable!("classify_state never returns Initial"),
    }
}

/// Probe loop — drives the state machine. Runs in tokio::spawn task.
/// Cadence: 30 s. Each tick: re-canonicalize, update healthy_roots,
/// detect transition, log + ntfy on change, then self-warm (touch each
/// healthy root once so the backing USB HDD doesn't spin down).
async fn run_state_machine_probe(state: Shared) {
    let probe_interval = Duration::from_secs(30);
    loop {
        sleep(probe_interval).await;

        let configured = state.configured_roots.clone();
        let configured_len = configured.len();
        let healthy_now =
            tokio::task::spawn_blocking(move || canonicalize_roots_quiet(&configured))
                .await
                .unwrap_or_default();
        let new_state = classify_state(healthy_now.len(), configured_len);

        // Update healthy_roots so the next /library/list call uses the
        // current set (cheap clone; readers always see latest).
        {
            let mut h = state
                .healthy_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *h = healthy_now.clone();
        }

        // Transition detection — atomic check + swap.
        let prev_state = {
            let mut last = state.last_state.lock().unwrap_or_else(|e| e.into_inner());
            let prev = *last;
            *last = new_state;
            prev
        };
        if prev_state != new_state {
            log_transition(prev_state, new_state, &healthy_now, &state.configured_roots);
            if !state.ntfy_url.is_empty() {
                send_ntfy_alert(
                    &state.ntfy_url,
                    prev_state,
                    new_state,
                    &healthy_now,
                    &state.configured_roots,
                )
                .await;
            }
        }

        // Self-warm: cheap directory-entry touch keeps the USB HDD spinning
        // (mirrors the v3.6.3 invariant — see the do_play liveness-gated
        // 25s match-timeout in server.rs which depends on the drive being
        // warm in steady state).
        for r in &healthy_now {
            let _ = std::fs::read_dir(r).map(|mut it| it.next());
        }
    }
}

/// Logged ONCE per state transition. Severity rises for degraded/waiting
/// (drive-loss events the operator wants visible at WARN).
fn log_transition(
    prev: LibraryStateKind,
    curr: LibraryStateKind,
    healthy: &[PathBuf],
    configured: &[PathBuf],
) {
    let high = matches!(curr, LibraryStateKind::Waiting | LibraryStateKind::Degraded);
    let msg = format!(
        "serve-library: state transition {:?} → {:?} ({}/{} roots healthy)",
        prev,
        curr,
        healthy.len(),
        configured.len()
    );
    if high {
        tracing::warn!("{}", msg);
    } else {
        tracing::info!("{}", msg);
    }
}

/// Best-effort ntfy POST on state transition. Never crashes the daemon
/// on failure — logs WARN once and continues. 3 s timeout so a wedged
/// ntfy can't stall the probe loop.
async fn send_ntfy_alert(
    url: &str,
    prev: LibraryStateKind,
    curr: LibraryStateKind,
    healthy: &[PathBuf],
    configured: &[PathBuf],
) {
    let (title, priority) = match curr {
        LibraryStateKind::Waiting => ("spela-library: all drives missing", "high"),
        LibraryStateKind::Degraded => ("spela-library: degraded", "default"),
        LibraryStateKind::Serving => ("spela-library: recovered", "default"),
        LibraryStateKind::Initial => return,
    };
    let body = format!(
        "{:?} -> {:?}  ({} of {} roots healthy)",
        prev,
        curr,
        healthy.len(),
        configured.len()
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "serve-library: ntfy client build failed: {} (best-effort, ignoring)",
                e
            );
            return;
        }
    };
    match client
        .post(url)
        .header("Title", title)
        .header("Priority", priority)
        .header("Tags", "spela,library,storage")
        .body(body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            tracing::info!(
                "serve-library: ntfy alert posted ({:?} -> {:?})",
                prev,
                curr
            );
        }
        Ok(r) => {
            tracing::warn!(
                "serve-library: ntfy POST returned {} (best-effort, ignoring)",
                r.status()
            );
        }
        Err(e) => {
            tracing::warn!(
                "serve-library: ntfy POST failed: {} (best-effort, ignoring)",
                e
            );
        }
    }
}

/// 503 + JSON shape returned by `/library/{list,match}` while in
/// `Waiting`. The JSON body tells the spela aggregator (and any future
/// monitoring) WHICH roots are missing so the operator can act
/// specifically, not just "origin offline".
fn waiting_response(configured: &[PathBuf]) -> Response {
    let missing: Vec<String> = configured
        .iter()
        .filter(|p| std::fs::canonicalize(p).is_err())
        .map(|p| p.display().to_string())
        .collect();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "library_drives_unavailable",
            "status": "waiting",
            "missing_roots": missing,
        })),
    )
        .into_response()
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
    let roots = state
        .healthy_roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if roots.is_empty() {
        // Waiting state — drives gone. Honest 503 + JSON instead of an
        // upstream-confusing 404 ("no local match" would be misleading).
        return waiting_response(&state.configured_roots);
    }
    let empty = std::collections::HashSet::new();
    // Reuse the EXACT server-side matcher + multi-root helper. expected
    // bytes = 0 (the origin host has no torrent-size expectation; the
    // title/year/quality + health matrix is the discriminator).
    // An explicit `file` means the caller already chose from `/library/files`:
    // join it under the named top-level directory instead of re-running the
    // title matcher (which would pick its own file inside a pack).
    let hit = if let Some(f) = p.file.as_deref().filter(|f| !f.trim().is_empty()) {
        roots
            .iter()
            .map(|r| r.join(&p.title).join(f))
            .find(|c| c.is_file())
    } else {
        first_local_bypass_match(&roots, &p.title, p.quality.as_deref(), 0, &empty)
    };
    let Some(path) = hit else {
        return (StatusCode::NOT_FOUND, "no local match").into_response();
    };
    let Some(canon) = resolve_under_roots(&roots, &path) else {
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
/// `GET /library/files?title=<raw_name>` — the media files inside one directory
/// release, name-sorted. Feeds the SPA's pack chooser: a card whose `file_count`
/// is >1 asks for this list, shows it, and plays the chosen entry by passing its
/// name back as `/library/match?title=&file=`. Never leaks an absolute path (same
/// posture as `/match`) — only the inner filename, which the caller already needs
/// in order to choose. 2026-08-26.
async fn handle_library_files(
    State(state): State<Shared>,
    Query(p): Query<MatchParams>,
) -> Response {
    if p.title.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "title required").into_response();
    }
    let roots = state
        .healthy_roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if roots.is_empty() {
        return waiting_response(&state.configured_roots);
    }
    let Some(dir) = roots
        .iter()
        .map(|r| r.join(&p.title))
        .find(|c| c.is_dir())
        .and_then(|c| resolve_under_roots(&roots, &c))
    else {
        return (StatusCode::NOT_FOUND, "no such release").into_response();
    };
    let files: Vec<serde_json::Value> = list_inner_media(&dir)
        .into_iter()
        .map(|(name, size, container)| json!({"name": name, "size": size, "container": container}))
        .collect();
    Json(json!({ "title": p.title, "files": files })).into_response()
}

async fn handle_library_list(State(state): State<Shared>) -> Response {
    let roots = state
        .healthy_roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if roots.is_empty() {
        return waiting_response(&state.configured_roots);
    }
    match tokio::task::spawn_blocking(move || enumerate_library(&roots)).await {
        Ok(entries) => {
            // DEBUG, not INFO — this fires every SPA library-view open
            // (3 s polling, web-remote AC-3.4). Steady-state silence is
            // the new invariant; the daemon logs only at startup + state
            // transitions.
            tracing::debug!("serve-library: /library/list -> {} entries", entries.len());
            Json(json!({ "entries": entries })).into_response()
        }
        Err(e) => {
            tracing::warn!("serve-library: /library/list join error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "enumeration failed").into_response()
        }
    }
}

/// `GET /health` — operational state snapshot for monitoring / external
/// checks (sluss / sentinel / Darwin's `/library` aggregator). Always
/// 200; the JSON `status` field tells the caller whether the drives
/// are actually serving. Distinct from `/library/list`'s 503 because
/// monitoring needs reachable-AND-state-aware liveness; the SPA reads
/// `/library/list` and the aggregator pings `/library/stream?h=<sentinel>`
/// — `/health` is the third oracle for things that want both signals.
async fn handle_health(State(state): State<Shared>) -> Response {
    let healthy = state
        .healthy_roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let curr = *state.last_state.lock().unwrap_or_else(|e| e.into_inner());
    let status_str = match curr {
        LibraryStateKind::Serving => "serving",
        LibraryStateKind::Degraded => "degraded",
        LibraryStateKind::Waiting => "waiting",
        LibraryStateKind::Initial => "initial",
    };
    Json(json!({
        "status": status_str,
        "configured_count": state.configured_roots.len(),
        "healthy_count": healthy.len(),
        "healthy_roots": healthy.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
    }))
    .into_response()
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
        match map.get_mut(&p.h) {
            Some((path, issued)) => {
                // Sliding TTL: an actively-streamed / seeked handle refreshes here, so
                // it never expires mid-watch (VLC re-opens the connection on each seek).
                *issued = Instant::now();
                path.clone()
            }
            None => return (StatusCode::GONE, "unknown or expired handle").into_response(),
        }
    };
    // Defense in depth: re-validate the path is still under a root and not
    // a symlink, every request (TOCTOU + handle-map integrity). If the
    // drive vanished mid-stream, healthy_roots is empty and resolve fails
    // — surfaces as 403 (security-boundary), matching pre-3.8 behaviour
    // when canonicalize would have failed for the same reason.
    let roots_now = state
        .healthy_roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(canon) = resolve_under_roots(&roots_now, &path) else {
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

    // ----- v3.8.0 drive-aware state machine -----

    #[test]
    fn classify_state_all_healthy_is_serving() {
        assert_eq!(classify_state(3, 3), LibraryStateKind::Serving);
        assert_eq!(classify_state(1, 1), LibraryStateKind::Serving);
    }

    #[test]
    fn classify_state_partial_is_degraded() {
        assert_eq!(classify_state(1, 2), LibraryStateKind::Degraded);
        assert_eq!(classify_state(2, 3), LibraryStateKind::Degraded);
    }

    #[test]
    fn classify_state_none_healthy_is_waiting() {
        assert_eq!(classify_state(0, 1), LibraryStateKind::Waiting);
        assert_eq!(classify_state(0, 5), LibraryStateKind::Waiting);
    }

    #[test]
    fn classify_state_zero_configured_defensive_waiting() {
        // Precondition is `configured > 0` (run() bails first), but the
        // defensive arm must NOT misclassify into Serving — that would
        // silently mean "you have nothing serving but I'll pretend it's
        // fine", which is exactly the failure-as-no-signal mode this
        // refactor exists to eliminate.
        assert_eq!(classify_state(0, 0), LibraryStateKind::Waiting);
    }

    #[test]
    fn canonicalize_roots_quiet_drops_missing_without_warning() {
        // The "without warning" property is verified by inspection of the
        // function body (no tracing::warn! call) — same behaviour against
        // the same input as the old canonicalize_roots, just silent so
        // 30s-cadence probes don't flood the log.
        let real = tempfile::tempdir().unwrap();
        let real_path = real.path().to_path_buf();
        let missing = PathBuf::from("/nonexistent/path/that/definitely/does/not/exist");
        let raw = vec![real_path.clone(), missing];
        let got = canonicalize_roots_quiet(&raw);
        assert_eq!(got.len(), 1, "missing path dropped, real path kept");
        assert_eq!(got[0], std::fs::canonicalize(&real_path).unwrap());
    }

    #[test]
    fn canonicalize_roots_quiet_handles_empty_input() {
        let got = canonicalize_roots_quiet(&[]);
        assert!(got.is_empty());
    }

    #[test]
    fn canonicalize_roots_quiet_all_missing_returns_empty() {
        // The "BOHR vanished" case — every configured root fails. The
        // result drives the state machine into Waiting (verified by the
        // classify_state tests above).
        let raw = vec![
            PathBuf::from("/nope/one"),
            PathBuf::from("/nope/two"),
            PathBuf::from("/nope/three"),
        ];
        let got = canonicalize_roots_quiet(&raw);
        assert!(got.is_empty());
        // Sanity: the resulting state IS Waiting.
        assert_eq!(
            classify_state(got.len(), raw.len()),
            LibraryStateKind::Waiting
        );
    }
}

#[cfg(test)]
mod pack_chooser_tests {
    use super::*;

    /// Real release names copied from the BOHR library (the Game of Thrones
    /// directory that prompted this: named "S01 S02 S03 Complete" but holding a
    /// single S03E05 file) plus a genuine multi-episode pack shape.
    fn pack(dir: &Path, names: &[&str]) {
        for n in names {
            let p = dir.join(n);
            std::fs::write(&p, vec![0u8; (MIN_LIBRARY_FILE_BYTES + 1) as usize]).unwrap();
        }
    }

    #[test]
    fn list_inner_media_is_episode_sorted_and_filtered() {
        let d = tempfile::tempdir().unwrap();
        pack(
            d.path(),
            &[
                "Girls.S01E03.mkv",
                "Girls.S01E01.mkv",
                "Girls.S01E02.mkv",
                "transcoded_leftover.mkv",
                "I.nfo",
            ],
        );
        let got = list_inner_media(d.path());
        let names: Vec<&str> = got.iter().map(|(n, _, _)| n.as_str()).collect();
        // name-sorted => episode order; the `transcoded` file and the non-video
        // .nfo are both excluded.
        assert_eq!(
            names,
            vec!["Girls.S01E01.mkv", "Girls.S01E02.mkv", "Girls.S01E03.mkv"]
        );
    }

    #[test]
    fn a_directory_with_one_file_is_not_a_pack() {
        // The anchoring case: a dir NAMED as a 3-season pack that actually holds
        // one episode must NOT show a chooser — file_count stays None.
        let d = tempfile::tempdir().unwrap();
        pack(d.path(), &["S03E05.1080p.WEB-DL.x264.anoXmous_.mp4"]);
        assert_eq!(list_inner_media(d.path()).len(), 1);
    }

    #[test]
    fn list_inner_media_ignores_subdirectories() {
        let d = tempfile::tempdir().unwrap();
        pack(d.path(), &["Show.S01E01.mkv"]);
        std::fs::create_dir(d.path().join("Subs")).unwrap();
        pack(&d.path().join("Subs"), &["Show.S01E01.mkv"]);
        assert_eq!(list_inner_media(d.path()).len(), 1);
    }
}
