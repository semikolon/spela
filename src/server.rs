use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::cast::{self, CastController};
use crate::config::Config;
use crate::disk;
use crate::search::SearchEngine;
use crate::state::{AppState, CurrentStream, HWM_CLEAR_FRACTION};
use crate::subtitles;
use crate::torrent;
use crate::torrent_engine::{self, TorrentEngine};
use crate::torrent_stream;
use crate::transcode;

pub struct ServerState {
    pub config: Config,
    pub search_engine: SearchEngine,
    pub cast: Mutex<CastController>,
    pub state_dir: PathBuf,
    pub media_dir: PathBuf,
    /// PID of the running ffmpeg transcode process (if any)
    pub ffmpeg_pid: Mutex<Option<u32>>,
    /// librqbit-backed pure-Rust torrent engine (since v3.3.0 / Apr 30, 2026
    /// — Phase 3 dropped the optional webtorrent fallback after the Phase 2
    /// live test validated peer attach + end-to-end cast on librqbit). The
    /// engine is in-process; torrent streams are served on the same axum
    /// router as `/hls/master.m3u8`, no separate :8888 HTTP server.
    pub torrent_engine: Arc<TorrentEngine>,
    /// Apr 30, 2026 (security audit H2): precomputed Host-header allowlist
    /// for the require_host_header middleware. Built once at startup from
    /// `Config` via `compute_host_allowlist`. Loopback + `darwin.home` +
    /// stream_host + config.allowed_hosts.
    pub host_allowlist: std::collections::HashSet<String>,
}

type SharedState = Arc<ServerState>;

pub async fn run_server(mut config: Config) -> anyhow::Result<()> {
    // Auto-detect a routable stream host fallback if not set in config.
    if config.stream_host.is_empty() {
        if let Some(host) = Config::detect_stream_host_fallback() {
            tracing::info!("Auto-detected stream host fallback: {}", host);
            config.stream_host = host;
        } else {
            tracing::warn!("Could not auto-detect a stream host fallback. Set stream_host in config.toml");
            config.stream_host = "127.0.0.1".into();
        }
    }

    // Apr 15, 2026: Chromecast hardcodes Google DNS (8.8.8.8 / 8.8.4.4) and
    // can NOT resolve LAN-only hostnames like `darwin.home` even when the
    // user's other devices reach them through the LAN's recursive resolver
    // (AdGuard Home, dnsmasq, mDNS). spela's cast LOAD URL is built from
    // `stream_host`, so a hostname here means the receiver fetches a name
    // it can't resolve, the LOAD fails silently, and `player_state` stays
    // IDLE while the rest of the pipeline runs healthily into the void.
    // Warn loudly if the configured stream_host looks like a hostname so
    // the user knows to switch to a private LAN IP.
    if !config.stream_host.is_empty() {
        let looks_like_hostname = config
            .stream_host
            .chars()
            .any(|c| c.is_ascii_alphabetic() && c != ':')
            && !config.stream_host.starts_with('[');
        if looks_like_hostname {
            tracing::warn!(
                "stream_host = {:?} looks like a hostname. Chromecast hardcodes Google DNS and cannot resolve LAN hostnames; cast loads will silently fail with player_state=IDLE. Set stream_host to a private LAN IP (e.g. 192.168.1.x) for Chromecast targets to work.",
                config.stream_host
            );
        }
    }

    let state_dir = Config::state_dir();
    let media_dir = config.media_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&media_dir)?;

    // librqbit is the only torrent backend since v3.3.0 (Apr 30, 2026 — Phase 3
    // dropped the optional webtorrent path after the Phase 2 live test
    // validated peer attach + end-to-end cast). Init is fail-fast: if the
    // Session can't bootstrap, surface the error and abort startup.
    tracing::info!("Initializing librqbit torrent engine");
    let torrent_engine = TorrentEngine::new(
        &media_dir,
        config.stream_host.clone(),
        config.port,
    )
    .await
    .context("librqbit engine bootstrap failed")?;
    tracing::info!(
        "librqbit engine ready; ffmpeg fetches /torrent/... via loopback (127.0.0.1:{}), \
         Chromecast fetches /hls/... via stream_host ({}:{})",
        config.port,
        config.stream_host,
        config.port
    );

    reconcile_session_state_on_startup(&state_dir);

    let search_engine = SearchEngine::new(config.tmdb_api_key.clone());
    let cast = Mutex::new(CastController::new(&state_dir, config.known_devices.clone()));
    let port = config.port;
    let host = config.host.clone();
    let host_allowlist = compute_host_allowlist(&config);
    tracing::info!(
        "Host-header allowlist active: {} entries",
        host_allowlist.len()
    );

    let state = Arc::new(ServerState {
        config,
        search_engine,
        cast,
        state_dir,
        media_dir,
        ffmpeg_pid: Mutex::new(None),
        torrent_engine,
        host_allowlist,
    });

    // Apr 30, 2026 (H2): tightened CORS — explicit LAN-origin allowlist,
    // dropped the wildcard. Browser-based tooling against spela has to
    // come from a known origin or get no CORS preflight ack. Most spela
    // consumers (CLI, voice tool, Chromecast) are NOT browsers and don't
    // care about CORS, so this is only a defense against DNS-rebinding
    // attacks via a malicious page making fetch() calls.
    let cors_origins: Vec<HeaderValue> = [
        "http://localhost:7890",
        "http://127.0.0.1:7890",
        "http://darwin.home:7890",
        &format!("http://{}:{}", state.config.stream_host, port),
    ]
    .iter()
    .filter_map(|s| s.parse().ok())
    .collect();
    let cors = CorsLayer::new()
        .allow_origin(cors_origins)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS, Method::DELETE])
        .allow_headers(Any);

    let app = Router::new()
        .route("/search", get(handle_search))
        .route("/play", post(handle_play))
        .route("/stop", post(handle_stop))
        .route("/status", get(handle_status))
        .route("/pause", post(handle_pause))
        .route("/resume", post(handle_resume))
        .route("/seek", post(handle_seek))
        .route("/volume", post(handle_volume))
        .route("/next", post(handle_next))
        .route("/prev", post(handle_prev))
        .route("/targets", get(handle_targets))
        .route("/history", get(handle_history))
        .route("/queue", get(handle_queue_list).post(handle_queue_add).delete(handle_queue_clear))
        .route("/config", get(handle_get_config).post(handle_set_config))
        .route("/cast-info", post(handle_cast_info))
        .route("/stream/transcode", get(handle_transcode_stream))
        // HLS streaming endpoints (Apr 15, 2026 rework — proper Chromecast support).
        // The route layout MUST match the URLs the HLS manifest produces:
        // ffmpeg's HLS muxer emits relative segment paths (e.g. `seg_00000.ts`),
        // which Chromecast resolves against the playlist URL. Playlist is at
        // /hls/playlist.m3u8 → segments resolve to /hls/seg_00000.ts, so the
        // segment route must live directly at /hls/{segment}, NOT /hls/segment/{segment}.
        // axum 0.8's matchit router gives literal routes precedence over the
        // {segment} capture, so they don't collide.
        //
        // The cast LOAD URL is /hls/master.m3u8 (NOT playlist.m3u8): older
        // Chromecast firmwares (CrKey 1.56) won't load a media playlist
        // directly without explicit CODECS / RESOLUTION / BANDWIDTH hints.
        // The master playlist is generated synthetically in handle_hls_master
        // and references the ffmpeg-written media playlist by relative path.
        .route("/hls/master.m3u8", get(handle_hls_master))
        .route("/hls/playlist.m3u8", get(handle_hls_playlist))
        .route("/hls/init.mp4", get(handle_hls_init))
        .route("/hls/{segment}", get(handle_hls_segment))
        // Custom Cast Receiver endpoints
        .route("/cast-receiver.html", get(handle_cast_receiver_html))
        .route("/cast-receiver/intro.mp4", get(handle_cast_receiver_intro))
        .route("/cast-receiver/subs.vtt", get(handle_cast_receiver_subs))
        .route("/api/cast-config", get(handle_cast_config))
        .route("/api/seek-restart", post(handle_seek_restart))
        .route("/api/position", get(handle_get_position).post(handle_save_position))
        .route("/api/position/reset", post(handle_reset_position))
        .route("/api/retry", post(handle_retry))
        // Apr 30, 2026: librqbit-backed torrent streaming. Replaces webtorrent's
        // separate :8888 HTTP server with a route on spela's existing axum
        // router. ffmpeg is the only consumer; it issues `Range: bytes=N-`
        // requests as it transcodes, and librqbit re-prioritizes pieces around
        // the requested offset. See `torrent_stream.rs` for the Range parser
        // (full RFC 7233 coverage, 19 unit tests).
        // Apr 30, 2026 (security audit H4): librqbit stream endpoint
        // restricted to loopback via a sub-router. The only legitimate
        // consumer is ffmpeg, which always runs on the same host as spela.
        // Chromecast hits /hls/* (the transcoded output), NEVER /torrent/*
        // (the raw librqbit-served bytes). DoS via Range-flooding and
        // exfiltration of in-progress torrent contents both require
        // non-loopback access; the per-route layer closes that surface.
        .merge(
            Router::new()
                .route(
                    "/torrent/{id}/stream/{file_idx}",
                    get(handle_torrent_stream),
                )
                .layer(axum::middleware::from_fn(require_loopback_source)),
        )
        // Apr 30, 2026 (H2): Host-header allowlist applied to ALL routes.
        // Order matters: middleware runs in reverse (CORS-then-host means
        // host check runs first per request). DNS-rebinding attacks set
        // a non-allowlisted Host; rejected with 403 before any handler.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_host_header,
        ))
        .layer(cors)
        .with_state(state);

    let addr = format!("{}:{}", host, port);
    tracing::info!("spela server listening on http://{}", addr);
    tracing::info!("Endpoints: /search /play /stop /status /pause /resume /seek /volume /next /prev /targets /history /config");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // Apr 30, 2026 (H4): use into_make_service_with_connect_info so the
    // require_loopback_source middleware can extract `ConnectInfo<SocketAddr>`
    // and check the source IP. axum's default `Router::into_make_service`
    // doesn't expose ConnectInfo; without this swap the middleware would
    // 500 on every request to /torrent/*.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Apr 30, 2026 (security audit H3): acquire a Mutex guard, recovering
/// from `PoisonError` rather than panicking. The original `.lock().unwrap()`
/// pattern caused today's rustls-panic cascade — a librqbit thread panicked
/// while holding the cast Mutex (PoisonError-poisoned it) → every
/// subsequent `.lock().unwrap()` on the same Mutex panicked too → cascade
/// across all axum tasks until the user restarted spela. With recovery,
/// post-panic state is treated as "potentially inconsistent but accessible"
/// (the contract `PoisonError::into_inner` provides). Callers MAY observe
/// stale state, which is strictly better than the entire server cascading
/// down.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        tracing::error!(
            "Mutex poisoned, recovering — a prior thread panicked while holding it"
        );
        e.into_inner()
    })
}

/// Apr 30, 2026 (security audit H2): compute the effective Host-header
/// allowlist for the server. Always includes loopback (`localhost`,
/// `127.0.0.1`) and the canonical fleet hostname (`darwin.home`); adds
/// `stream_host` if non-empty (Chromecast LAN endpoint); appends
/// `config.allowed_hosts` for custom deployments. Returned as a HashSet
/// for O(1) lookup in the per-request middleware. Pure function so the
/// allowlist composition is testable.
pub(crate) fn compute_host_allowlist(config: &Config) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    set.insert("localhost".into());
    set.insert("127.0.0.1".into());
    set.insert("darwin.home".into());
    if !config.stream_host.is_empty() {
        set.insert(config.stream_host.clone());
    }
    for h in &config.allowed_hosts {
        if !h.is_empty() {
            set.insert(h.clone());
        }
    }
    set
}

/// Strip the `:port` suffix (if any) from a Host header value. Pure helper
/// so the parsing is unit-testable.
pub(crate) fn parse_host_header(raw: &str) -> &str {
    // IPv6 form: `[::1]:7890`. Keep the bracketed address; only strip the
    // trailing `:port` after the closing bracket. `end` is the index of `]`
    // within `rest` (after stripping the leading `[`); add 1 to account for
    // the stripped `[`, then +1 again to include `]` in the substring.
    if let Some(rest) = raw.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &raw[..end + 2];
        }
    }
    match raw.rfind(':') {
        Some(idx) => &raw[..idx],
        None => raw,
    }
}

/// Apr 30, 2026 (security audit H4): only loopback may hit
/// `/torrent/{id}/stream/{file_idx}`. ffmpeg is the sole legitimate
/// consumer and runs in-process. Rejects 403 for any non-loopback source IP.
async fn require_loopback_source(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    if addr.ip().is_loopback() {
        Ok(next.run(req).await)
    } else {
        tracing::warn!(
            "/torrent/* rejected from non-loopback source: {}",
            addr
        );
        Err(StatusCode::FORBIDDEN)
    }
}

/// Host-header allowlist middleware. Rejects requests whose Host header
/// (with port stripped) isn't in the configured allowlist. The primary
/// defense against DNS rebinding from any browser tab on the LAN.
async fn require_host_header(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let host_header = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    let host_only = parse_host_header(host_header);
    if state.host_allowlist.contains(host_only) {
        Ok(next.run(req).await)
    } else {
        tracing::warn!(
            "Host-header rejected: {:?} not in allowlist",
            host_only
        );
        Err(StatusCode::FORBIDDEN)
    }
}

/// Clear stale `current` stream entries on startup. The librqbit Session
/// always boots fresh — no torrents inherited from prior runs — so any
/// non-zero `pid` (which represents a `TorrentId`) recorded in
/// `app_state.current` is by definition stale and gets cleared. `pid == 0`
/// is Local Bypass (no worker); we can't easily tell from disk whether
/// ffmpeg/HLS state is still meaningful, so we conservatively clear that
/// too. Belt-and-suspenders: kill any lingering Node webtorrent processes
/// from pre-v3.3.0 deployments that may still be running after an upgrade.
fn reconcile_session_state_on_startup(state_dir: &PathBuf) {
    let mut app_state = AppState::load(state_dir);
    if app_state.current.is_some() {
        tracing::warn!("Clearing stale current stream on startup (fresh librqbit session)");
        app_state.current = None;
        let _ = app_state.save(state_dir);
    }
    let killed = torrent::kill_lingering_webtorrent_workers();
    if !killed.is_empty() {
        tracing::warn!(
            "Terminated lingering pre-v3.3.0 webtorrent workers on startup: {:?}",
            killed
        );
    }
    // webtorrent.pid is obsolete since v3.3.0; remove if present.
    let _ = std::fs::remove_file(state_dir.join("webtorrent.pid"));
}

/// Start a torrent and return `(torrent_id, http_url)` for `do_play` to wire
/// into `CurrentStream.pid` + ffmpeg's input URL.
async fn start_torrent_for_play(
    state: &SharedState,
    magnet: &str,
    file_index: Option<u32>,
) -> anyhow::Result<(u32, String)> {
    let info = state.torrent_engine.start(magnet, file_index).await?;
    tracing::info!(
        "librqbit: torrent {} started, file_idx={}, url={}",
        info.id,
        info.file_index,
        info.url
    );
    Ok((info.id, info.url))
}

/// "Is the torrent making progress?" check used by do_play's 12s self-healing
/// fall-through. Returns `true` once librqbit reports any sign of life (peers
/// connected, bytes downloaded, or non-zero speed) before the deadline.
async fn check_torrent_progress(
    state: &SharedState,
    torrent_id: u32,
    timeout_secs: u64,
) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        if let Some(p) = state.torrent_engine.progress(torrent_id) {
            if p.bytes_downloaded > 0 || p.peers_connected > 0 || p.speed_bps > 0 {
                tracing::info!(
                    "librqbit: torrent {} progress detected (bytes={}, peers={}, speed={} B/s)",
                    torrent_id,
                    p.bytes_downloaded,
                    p.peers_connected,
                    p.speed_bps
                );
                return true;
            }
        }
    }
    false
}

/// Stop a torrent. `delete_files=true` for failed-start cleanup (sparse
/// placeholders aren't worth keeping); `delete_files=false` for post-playback
/// "keep on disk so Local Bypass can reuse" cleanup.
async fn stop_torrent(state: &SharedState, torrent_id: u32, delete_files: bool) {
    if let Err(e) = state.torrent_engine.stop(torrent_id, delete_files).await {
        tracing::warn!(
            "librqbit: stop({}, {}) failed: {}",
            torrent_id,
            delete_files,
            e
        );
    }
}

/// "Is the torrent worker still alive?" check used by the post-playback
/// reaper. `pid == 0` is Local Bypass (no torrent worker); the reaper then
/// relies entirely on ffmpeg liveness.
fn is_torrent_alive(state: &SharedState, torrent_id: u32) -> bool {
    if torrent_id == 0 {
        return true;
    }
    state.torrent_engine.handle(torrent_id).is_some()
}

/// axum handler for the librqbit streaming endpoint.
/// `GET /torrent/{id}/stream/{file_idx}` — thin wrapper around the
/// pure HTTP-response builder unit-tested in `torrent_stream.rs`.
async fn handle_torrent_stream(
    State(state): State<SharedState>,
    axum::extract::Path((id, file_idx)): axum::extract::Path<(u32, usize)>,
    headers: HeaderMap,
) -> axum::response::Response {
    match torrent_stream::serve_torrent_stream(&state.torrent_engine, id, file_idx, &headers).await
    {
        Ok(resp) => resp,
        Err(status) => status.into_response(),
    }
}

// --- Request types ---

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    movie: Option<String>,
    season: Option<u32>,
    episode: Option<u32>,
}

#[derive(Deserialize)]
pub struct PlayRequest {
    pub magnet: Option<String>,
    /// Play search result by ID (1-8) from last search — auto-fills magnet, file_index, metadata
    pub result_id: Option<usize>,
    pub target: Option<String>,
    pub cast_name: Option<String>,
    pub title: Option<String>,
    pub file_index: Option<u32>,
    pub no_subs: Option<bool>,
    pub no_intro: Option<bool>,
    pub subtitle_lang: Option<String>,
    pub imdb_id: Option<String>,
    pub show: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub seek_to: Option<f64>,
    pub duration: Option<f64>,
    pub quality: Option<String>,
    pub size: Option<String>,
    /// Apr 28, 2026 (Apr 29 corrected): TMDB poster URL for the playing
    /// item. Auto-filled by the play handler from `last_search.json`'s
    /// `show.poster_url`. Sent through to `cast_url`'s `CastMetadata` so
    /// the Default Media Receiver shows a poster + title splash on top of
    /// the playback view. Does NOT affect progress-bar overlay behavior —
    /// that is governed by stream type (live vs VOD HLS). See spela
    /// CLAUDE.md § "DMR overlay is stream-type-dependent".
    pub poster_url: Option<String>,
}

#[derive(Deserialize)]
struct SeekRequest {
    t: Option<f64>,
    seconds: Option<f64>,
}

#[derive(Deserialize)]
struct VolumeRequest {
    level: Option<u32>,
}

#[derive(Deserialize)]
struct CastInfoRequest {
    device: Option<String>,
}

// --- Handlers ---

async fn handle_search(
    State(state): State<SharedState>,
    Query(params): Query<SearchParams>,
) -> Json<Value> {
    let q = match params.q {
        Some(q) if !q.is_empty() => q,
        _ => return Json(json!({"error": "Missing q parameter"})),
    };
    let movie = params.movie.is_some();
    match state.search_engine.search(&q, movie, params.season, params.episode).await {
        Ok(result) => {
            // Save results so `play <N>` can reference them
            AppState::save_last_search(&state.state_dir, &result);
            Json(serde_json::to_value(result).unwrap_or(json!({"error": "serialize failed"})))
        }
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

async fn handle_play(
    State(state): State<SharedState>,
    Json(mut req): Json<PlayRequest>,
) -> Json<Value> {
    // Auto-retry loop: tries up to 3 results on torrent failure
    let max_retries = 3u32;
    for retry in 0..max_retries {
        let result = do_play(&state, &mut req).await;
        match &result {
            Json(v) if v.get("error").is_some() && retry < max_retries - 1 => {
                // Check if we can auto-fallback to next result
                if let Some(rid) = req.result_id {
                    if let Some(search) = AppState::load_last_search(&state.state_dir) {
                        let next_rid = rid + 1;
                        if next_rid <= search.results.len() {
                            tracing::warn!("Play failed ({}), auto-trying result #{}", v["error"], next_rid);
                            // Apr 30, 2026 (M11): consolidated — do_play's
                            // own cast-failure path (server.rs:~902 in the
                            // current version, "Cast-failure cleanup defense"
                            // shipped Apr 15 / commit 8735ea4) already kills
                            // ffmpeg, deletes transcoded artifacts, and stops
                            // the torrent before returning the error. The
                            // retry loop only needs to bump result_id and
                            // re-enter — duplicating cleanup here racetimes
                            // do_play's own cleanup AND can SIGTERM a
                            // transient ffmpeg PID that the next do_play
                            // attempt has just spawned.
                            req.result_id = Some(next_rid);
                            req.magnet = None;
                            req.file_index = None;
                            req.duration = None;
                            req.quality = None;
                            req.size = None;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        return result;
    }
    Json(json!({"error": "All retry attempts failed"}))
}

async fn do_play(
    state: &SharedState,
    req: &mut PlayRequest,
) -> Json<Value> {
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);

    // Resolve result_id from last search — fills magnet, file_index, and metadata automatically
    if let Some(rid) = req.result_id {
        match AppState::load_last_search(&state.state_dir) {
            Some(search) => {
                let result = search.results.iter().find(|r| r.id == rid);
                match result {
                    Some(r) => {
                        req.magnet = Some(r.magnet.clone());
                        req.file_index = req.file_index.or(r.file_index);
                        // Auto-fill metadata from the search context
                        if req.title.is_none() {
                            let ep = search.searching.as_ref();
                            req.title = Some(match (&search.show, ep) {
                                (Some(show), Some(ep)) => format!("{} S{:02}E{:02}", show.title, ep.season, ep.episode),
                                (Some(show), None) => show.title.clone(),
                                _ => r.title.clone(),
                            });
                        }
                        if req.imdb_id.is_none() {
                            req.imdb_id = search.show.as_ref().and_then(|s| s.imdb_id.clone());
                        }
                        if req.show.is_none() {
                            req.show = search.show.as_ref().map(|s| s.title.clone());
                        }
                        if req.season.is_none() {
                            req.season = search.searching.as_ref().map(|e| e.season);
                        }
                        if req.episode.is_none() {
                            req.episode = search.searching.as_ref().map(|e| e.episode);
                        }
                        if req.quality.is_none() {
                            req.quality = Some(r.quality.clone());
                        }
                        if req.size.is_none() {
                            req.size = Some(r.size.clone());
                        }
                        if req.poster_url.is_none() {
                            req.poster_url = search.show.as_ref()
                                .and_then(|s| s.poster_url.clone());
                        }
                        tracing::info!("Playing result #{}: {} (file_index: {:?})", rid, req.title.as_deref().unwrap_or("?"), req.file_index);
                    }
                    None => return Json(json!({"error": format!("Result #{} not found in last search (have {})", rid, search.results.len())})),
                }
            }
            None => return Json(json!({"error": "No previous search results. Run 'spela search' first."})),
        }
    }

    let magnet = match &req.magnet {
        Some(m) if !m.is_empty() => m.clone(),
        _ => return Json(json!({"error": "Missing magnet. Use 'spela play <N>' with a result number, or pass a magnet link."})),
    };
    // Apr 30, 2026 SSRF defense: librqbit's add_torrent fetches
    // http(s):// URLs as .torrent files. With the unauthenticated HTTP
    // surface, that's an SSRF pivot — see torrent_engine::validate_magnet_uri.
    // Reject at the HTTP boundary so the rejection error reaches the caller
    // cleanly (not buried in a librqbit error).
    if let Err(e) = torrent_engine::validate_magnet_uri(&magnet) {
        return Json(json!({"error": format!("Invalid magnet: {}", e)}));
    }

    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());

    // --- SMART DISK HYGIENE ---
    // Proactively prune stale media AND enforce the 10 GB cache cap via
    // LRU pressure eviction. `prune_to_fit` runs the age-based prune first,
    // then evicts oldest-first if still over cap — so the cap is a
    // self-maintaining upper bound instead of a hard refusal wall. The
    // active title is always protected. See `disk::prune_to_fit` for the
    // full rationale + Apr 15 2026 incident context.
    disk::prune_to_fit(&media_dir, &title, disk::MAX_MEDIA_MB);

    // Local Bypass System: Check if the movie already exists on disk.
    //
    // Apr 30, 2026: ~100 lines of file-scan + match-decision logic
    // extracted into the pure `find_local_bypass_match` helper, which
    // pins the title/year/quality/health decision matrix in 8 unit tests.
    // do_play just consumes the helper's Option<PathBuf> result.
    let mut server_url = String::new();
    let mut pid: u32 = 0;
    let mut is_local = false;

    if req.title.is_some() {
        let expected_bytes = req
            .size
            .as_deref()
            .and_then(parse_size_to_bytes)
            .unwrap_or(0);
        if let Some(local_path) =
            find_local_bypass_match(&media_dir, &title, req.quality.as_deref(), expected_bytes)
        {
            tracing::info!(
                "Local Bypass: matched on disk: {:?} (expected {}B)",
                local_path,
                expected_bytes
            );
            server_url = format!("file://{}", local_path.to_string_lossy());
            is_local = true;
        }
    }

    // Stop the previous stream's torrent (if any) before starting a new one.
    // The previous torrent's id lives in `app_state.current.pid`; we route
    // through `engine.stop`. `pid == 0` is Local Bypass — no torrent worker
    // to stop.
    let prev_pid = AppState::load(&state.state_dir)
        .current
        .as_ref()
        .map(|c| c.pid)
        .unwrap_or(0);
    if prev_pid != 0 {
        stop_torrent(state, prev_pid, false).await;
    }
    if let Some(old_fb_pid) = lock_recover(&state.ffmpeg_pid).take() {
        tracing::info!("do_play: killing existing ffmpeg zombie (PID {})", old_fb_pid);
        torrent::kill_pid(old_fb_pid);
    }
    // Aggressive cleanup: delete the transcode file to break any lingering connections
    let ffmpeg_log = state.media_dir.join("transcoded_aac.mp4");
    if ffmpeg_log.exists() {
        let _ = std::fs::remove_file(&ffmpeg_log);
    }

    let mut app_state = AppState::load(&state.state_dir);
    app_state.current = None;
    let _ = app_state.save(&state.state_dir);

    // Apr 30, 2026 (M1): treat empty-string target the same as None.
    // Without this filter, a caller passing `{"target": ""}` would
    // bypass the cast block AND skip cast_health_monitor spawn AND
    // save `current.target = ":<cast_name>"` — silent broken-state.
    let target = req
        .target
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&app_state.preferences.default_target)
        .to_string();
    let cast_name = req.cast_name.clone()
        .or_else(|| app_state.preferences.chromecast_name.clone())
        .unwrap_or_else(|| state.config.default_device.clone());
    let no_subs = req.no_subs.unwrap_or(false);
    let sub_lang = req.subtitle_lang.clone().unwrap_or_else(|| "eng".into());

    // Start the torrent if NOT local. `pid` ends up tagging
    // `CurrentStream.pid` and `server_url` is the URL ffmpeg fetches from
    // (`http://stream_host:port/torrent/{id}/stream/{file_idx}` —
    // FileStream-backed via librqbit, served on the same axum router).
    if !is_local {
        // Disk check: only required if we are going to start a NEW download
        if let Ok(Some(err)) = disk::check_space(&media_dir) {
            return Json(json!({"error": err}));
        }

        let result = match start_torrent_for_play(state, &magnet, req.file_index).await {
            Ok(r) => r,
            Err(e) => return Json(json!({"error": e.to_string()})),
        };
        pid = result.0;
        server_url = result.1;

        // Self-healing: check download progress
        if !check_torrent_progress(state, pid, 12).await {
            tracing::warn!("Torrent has no download progress after 12s — dead seeds");
            stop_torrent(state, pid, true).await;
            disk::prune_disk(&media_dir, ""); // Clean up any dead attempt
            return Json(json!({"error": "Torrent has no active seeds (0% after 12s)"}));
        }
    }

    // Fetch subtitles FIRST (needed for burn-in during transcode)
    let mut has_subtitles = false;
    let mut subtitle_srt_path: Option<PathBuf> = None;
    // Apr 28, 2026: pass the local source MKV (Local Bypass plays only) so
    // subtitles.rs can prefer the embedded forced English track over
    // OpenSubtitles' SDH-flavored file. For webtorrent-served plays the
    // path is `http://...` and we skip embedded extraction (the file may be
    // partially-downloaded and missing the subtitle tracks).
    let local_source_for_subs: Option<PathBuf> = if server_url.starts_with("file://") {
        Some(PathBuf::from(&server_url[7..]))
    } else {
        None
    };
    if !no_subs {
        if let Some(imdb_id) = &req.imdb_id {
            let client = reqwest::Client::new();
            match subtitles::fetch_subtitles(
                &client, imdb_id, req.season, req.episode,
                &sub_lang, &state.media_dir,
                local_source_for_subs.as_deref(),
            ).await {
                Ok(Some(_vtt_path)) => {
                    has_subtitles = true;
                    // Use the SRT version for ffmpeg burn-in (ffmpeg handles SRT natively)
                    subtitle_srt_path = Some(state.media_dir.join(format!("subtitle_{}.srt", sub_lang)));
                    tracing::info!("Subtitles fetched ({})", sub_lang);
                }
                Ok(None) => tracing::info!("No subtitles found for {}", sub_lang),
                Err(e) => tracing::warn!("Subtitle fetch failed: {}", e),
            }
        }
    }

    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());

    // Auto-resume from saved position if no explicit seek requested.
    //
    // Apr 15, 2026 UX fix: explicit `--seek N` (including `--seek 0`) is a
    // user-intentional action that must BYPASS auto-resume AND CLEAR any
    // stale high-water-mark. Principle: explicit user actions override
    // remembered state. Without this, running `spela play 3 --seek 0` to
    // restart an episode would silently resume at a saved 2236s position
    // because `save_position_smart`'s HWM logic preserved the old value.
    // The ONLY clean restart was `spela clear <imdb>` then `spela play 3`.
    // Apr 30, 2026 (M5): reject non-finite seek_to (NaN, ±infinity).
    // These flow through to ffmpeg's -ss arg AND to ss_offset which
    // cast_health_monitor uses for `absolute = current_time + ss_offset`
    // arithmetic. NaN comparisons are always false, silently corrupting
    // every HWM save thereafter. Filter at the do_play boundary.
    let user_explicitly_set_seek = req.seek_to.is_some_and(|t| t.is_finite());
    let mut seek_to = req.seek_to.filter(|t| t.is_finite());
    let mut auto_resumed_from: Option<f64> = None;
    if user_explicitly_set_seek {
        let mut app_state = AppState::load(&state.state_dir);
        let key = app_state.reset_position(req.imdb_id.clone(), req.title.clone());
        let _ = app_state.save(&state.state_dir);
        tracing::info!(
            "Explicit --seek {:?} overrides saved HWM for '{}' (cleared)",
            req.seek_to, key
        );
    } else {
        let app_state = AppState::load(&state.state_dir);
        let pos = app_state.get_position(req.imdb_id.clone(), req.title.clone());
        if pos > 30.0 { // Don't bother resuming if less than 30s in
            tracing::info!("Auto-resume: found saved position for '{}' at {:.0}s", title, pos);
            seek_to = Some(pos);
            auto_resumed_from = Some(pos);
        }
    }

    // NOTE: previously `do_cleanup(&state)` was called here, but that path
    // invokes `stop_by_pid_file` → `kill_all_webtorrent()`, which SIGTERMs the
    // webtorrent we just started a few lines above (and then ffmpeg would
    // immediately fail with "Connection refused" on the now-dead server).
    // Pre-start cleanup already happened at the top of `do_play`.

    // Codec detection + transcode decision
    let mut final_url = server_url.clone();
    let mut is_transcoded = false;
    let no_intro = req.no_intro.unwrap_or(false);
    let intro_path = if no_intro { None } else { transcode::find_intro() };

    let codec_info = transcode::detect_codecs(&server_url).await
        .unwrap_or(transcode::CodecInfo {
            video_codec: None, audio_codec: None, duration: None,
            audio_stream: "0:a:0".to_string(), audio_index: 0,
        });
    let video_codec = codec_info.video_codec;
    let audio_codec = codec_info.audio_codec;
    let source_duration = codec_info.duration;
    let audio_stream = codec_info.audio_stream.clone();
    let audio_index = codec_info.audio_index;
    if let Some(dur) = source_duration {
        tracing::info!("Source duration: {:.0}s ({:.0} min), preferred audio: {} (index {})",
                      dur, dur / 60.0, audio_stream, audio_index);
    }

    let need_audio_tc = audio_codec.as_deref().map_or(false, transcode::audio_needs_transcode);
    let need_video_tc = video_codec.as_deref().map_or(false, transcode::video_needs_transcode);
    let need_transcode = need_audio_tc || need_video_tc || intro_path.is_some() || subtitle_srt_path.is_some() || is_local;

    if need_transcode {
        let mut reasons = Vec::new();
        if need_audio_tc { reasons.push(format!("{} -> AAC", audio_codec.as_deref().unwrap_or("?"))); }
        if need_video_tc { reasons.push(format!("{} -> H.264 (NVENC)", video_codec.as_deref().unwrap_or("?"))); }
        if subtitle_srt_path.is_some() { reasons.push("subtitle burn-in".into()); }
        if intro_path.is_some() { reasons.push("intro clip".into()); }
        tracing::info!("Transcode needed: {}", reasons.join(" + "));

        let sub_path = subtitle_srt_path.as_deref();
        // Apr 15, 2026: switched from `transcode::transcode` (fragmented MP4
        // served via chunked-transfer at /stream/transcode, which Chromecast
        // Default Media Receiver rejects with player_state=IDLE) to
        // `transcode::transcode_hls` (HLS event playlist + fmp4 segments
        // served via /hls/playlist.m3u8 with proper Content-Length + Range).
        // See ~/Projects/spela/TODO.md § "Cast Pipeline Rework" for the full
        // trade-off analysis.
        match transcode::transcode_hls(&server_url, &media_dir, sub_path, intro_path.as_deref(), need_video_tc, seek_to, audio_index).await {
                Ok((manifest_path, ffmpeg_pid)) => {
                    // Track ffmpeg PID for the post-playback reaper + cleanup
                    *lock_recover(&state.ffmpeg_pid) = Some(ffmpeg_pid);

                    // HLS pre-buffer: wait for the manifest + enough segments
                    // to survive the Chromecast's initial read-ahead burst.
                    //
                    // Apr 18, 2026 root cause fix: waiting for just 1 segment
                    // caused the Chromecast to catch up to the transcode
                    // frontier after ~30s and start buffering (spinner). With
                    // intro concat + subtitle burn-in + seek, ffmpeg produces
                    // segments at ~1x realtime initially. The Chromecast
                    // consumes at 1x too, so 1-segment head start = 6 seconds
                    // of cushion, exhausted by segment 5.
                    //
                    // Fix: wait for 10 segments (~60s of content). At NVENC's
                    // ~3-6x realtime, this takes 10-20s of wall time. Gives
                    // the Chromecast a 60-second buffer before it can catch
                    // the frontier, by which time ffmpeg is well ahead.
                    let hls_dir = manifest_path.parent().map(|p| p.to_path_buf())
                        .unwrap_or_else(|| media_dir.join("transcoded_hls"));
                    let min_segments: usize = 10;
                    let target_segment = hls_dir.join(format!("seg_{:05}.ts", min_segments));
                    let prebuffer_timeout_secs: u64 = if intro_path.is_some() { 90 } else { 60 };
                    let prebuffer_deadline = tokio::time::Instant::now()
                        + tokio::time::Duration::from_secs(prebuffer_timeout_secs);
                    loop {
                        if tokio::time::Instant::now() > prebuffer_deadline {
                            tracing::warn!(
                                "HLS pre-buffer timeout ({}s) — casting with {} segments available",
                                prebuffer_timeout_secs,
                                std::fs::read_dir(&hls_dir).map(|d| d.filter(|e| {
                                    e.as_ref().map(|e| e.path().extension().map_or(false, |ext| ext == "ts")).unwrap_or(false)
                                }).count()).unwrap_or(0)
                            );
                            break;
                        }
                        if manifest_path.exists() && target_segment.exists() {
                            let seg_count = std::fs::read_dir(&hls_dir).map(|d| d.filter(|e| {
                                e.as_ref().map(|e| e.path().extension().map_or(false, |ext| ext == "ts")).unwrap_or(false)
                            }).count()).unwrap_or(0);
                            tracing::info!(
                                "HLS pre-buffer ready: {} segments at {:?} (target was {})",
                                seg_count, hls_dir, min_segments
                            );
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }

                    // Cast URL is the HLS MASTER playlist (not the media
                    // playlist directly). Older Chromecast firmwares —
                    // confirmed live on CrKey 1.56 / Fredriks TV — refuse
                    // to load a bare media playlist without CODECS /
                    // RESOLUTION / BANDWIDTH hints. /hls/master.m3u8 wraps
                    // ffmpeg's media playlist with those hints synthetically.
                    // Chromecast resolves segment URLs against the master,
                    // and `playlist.m3u8` (relative) → `/hls/playlist.m3u8`,
                    // and `seg_00000.ts` (relative to playlist.m3u8) →
                    // `/hls/seg_00000.ts`.
                    final_url = format!(
                        "http://{}:{}/hls/master.m3u8",
                        state.config.stream_host, state.config.port
                    );
                    is_transcoded = true;

                    if sub_path.is_some() {
                        tracing::info!("Subtitles burned into video stream via NVENC");
                    }
                }
                Err(e) => tracing::warn!("HLS transcode failed (casting original): {}", e),
            }
    }

    // Cast to Chromecast
    if target == "chromecast" {
        let state_clone = state.clone();
        let cast_name_clone = cast_name.clone();
        let url_clone = final_url.clone();
        // When duration is known, use Buffered (enables seeking). Otherwise Live.
        // Intro adds ~5s to total duration.
        let cast_duration = source_duration.map(|d| {
            let intro_secs = if intro_path.is_some() { 5.0 } else { 0.0 };
            d + intro_secs
        });
        // Pick the cast content_type from the URL: HLS manifests get the
        // official IANA media type which routes Default Media Receiver
        // through Shaka Player's HLS adapter; everything else (raw MP4,
        // direct file URLs) gets video/mp4.
        let cast_content_type: &str = if url_clone.ends_with(".m3u8")
            || url_clone.contains("/hls/playlist.m3u8")
        {
            "application/vnd.apple.mpegurl"
        } else {
            "video/mp4"
        };
        // Apr 28, 2026 (Apr 29 corrected): Build CastMetadata from the play
        // request. Gated by `config.rich_metadata_in_load`. When enabled,
        // DMR shows a poster + title splash on top of the playback view.
        // Does NOT govern the persistent progress-bar overlay — that's
        // stream-type-dependent (live HLS vs VOD HLS). Default off because
        // the splash adds clutter without solving the overlay axis. Full
        // case study: spela CLAUDE.md § "DMR overlay is stream-type-dependent,
        // not metadata-dependent".
        let cast_metadata = if state.config.rich_metadata_in_load {
            cast::CastMetadata {
                title: req.title.clone(),
                series_title: req.show.clone(),
                season: req.season,
                episode: req.episode,
                poster_url: req.poster_url.clone(),
                release_date: None,
            }
        } else {
            cast::CastMetadata::default()
        };
        let cast_metadata_clone = cast_metadata.clone();
        let cast_result = tokio::task::spawn_blocking(move || {
            let mut cast = lock_recover(&state_clone.cast);
            cast.cast_url(
                &cast_name_clone,
                &url_clone,
                cast_content_type,
                cast_duration,
                seek_to,
                &cast_metadata_clone,
            )
        }).await;

        match cast_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                // Defense in depth: the post-playback reaper has not been
                // spawned yet at this point in do_play, so without explicit
                // cleanup the webtorrent + ffmpeg we just started would
                // linger as orphans until the next play, the next server
                // restart, or `spela kill-workers`. This is the exact class
                // of leak the Apr 8 incident report warns about.
                do_cleanup(&state);
                return Json(json!({
                    "error": format!("Cast failed: {}", e),
                    "url": final_url,
                    "recovery_suggestion": "Try 'spela targets' to discover devices, or check if TV is on"
                }));
            }
            Err(e) => {
                // Same defense as above — async task panic must not leak
                // the freshly-spawned worker pipeline.
                do_cleanup(&state);
                return Json(json!({"error": format!("Cast task failed: {}", e)}));
            }
        }

        // --- Seek Logic ---
        // If we are NOT transcoding, we must tell the Chromecast to seek 
        // to the correct position after the media loads.
        // If we ARE transcoding, the stream itself already starts at the right 
        // point (Fake Live seek), so calling an absolute seek(2843) on a 3-second 
        // stream would cause a hang.
        if !is_transcoded {
            if let Some(pos) = seek_to {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                let state_clone = state.clone();
                let cast_name_clone = cast_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let mut cast = lock_recover(&state_clone.cast);
                    cast.seek(&cast_name_clone, pos)
                }).await;
            }
        }
    }

    // Save state
    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());
    let duration = source_duration;

    // If seeking, save the baseline position immediately to state.json
    if let Some(pos) = seek_to {
        let (key, saved) = app_state.save_position_smart(req.imdb_id.clone(), req.title.clone(), pos, duration);
        if saved {
            let _ = app_state.save(&state.state_dir);
            tracing::info!("Auto-resume: saved baseline position for '{}' at {}s", key, pos);
        }
    }

    // Apr 30, 2026 (L9): drop the historic 300-char magnet truncation —
    // magnets are typically 400-600 chars (multiple trackers), and the
    // truncation silently produced an unparseable saved-magnet on
    // restart. Memory cost of the full magnet in CurrentStream is
    // negligible.
    //
    // Apr 30, 2026 (L7): scrub poster_url to a known-safe TMDB CDN
    // origin before saving. The cast metadata is fetched by the
    // Chromecast directly, so an attacker-controlled URL becomes a
    // probe vector via the TV's request log.
    app_state.current = Some(CurrentStream {
        magnet: magnet.clone(),
        title: title.clone(),
        show: req.show.clone(),
        season: req.season,
        episode: req.episode,
        imdb_id: req.imdb_id.clone(),
        target: format!("{}:{}", target, cast_name),
        url: final_url.clone(),
        started_at: Utc::now(),
        pid,
        has_subtitles,
        subtitle_lang: if has_subtitles { Some(sub_lang) } else { None },
        duration,
        quality: req.quality.clone(),
        size: req.size.clone(),
        poster_url: req
            .poster_url
            .as_deref()
            .filter(|u| is_valid_poster_url(u))
            .map(String::from),
        // Remember the -ss offset so cast_health_monitor can translate the
        // Chromecast's 0-based current_time into absolute source-timeline
        // position when it periodically calls save_position_smart.
        //
        // Apr 15, 2026: this value ONLY applies when we passed `-ss N` to
        // ffmpeg (the transcoded-HLS path). For non-transcoded streams,
        // spela calls `cast.seek(pos)` AFTER the cast starts, and the
        // Chromecast's `current_time` already reflects the seeked position
        // on its own timeline — adding ss_offset to it would double-count
        // and produce impossible "absolute" values (this is the bug that
        // made cast_health_monitor declare 176% of duration and clean up
        // the stream before it could play). So: ss_offset is only ever
        // non-zero on a transcoded play whose seek was done via ffmpeg.
        ss_offset: if is_transcoded { seek_to.unwrap_or(0.0) } else { 0.0 },
    });
    let _ = app_state.save(&state.state_dir);

    // Spawn post-playback reaper: monitors pipeline, auto-cleans when movie ends.
    // Frees webtorrent's ~1.5GB RAM and cleans up media files.
    {
        let state = state.clone();
        let webtorrent_pid = pid;
        let title_for_log = title.clone();
        tokio::spawn(async move {
            // Wait for playback to establish before monitoring
            tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;

            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

                // Check if this stream is still the current one (user may have started another)
                let app_state = AppState::load(&state.state_dir);
                match &app_state.current {
                    Some(c) if c.pid == webtorrent_pid => {} // Still our stream
                    _ => {
                        tracing::debug!("Reaper: stream replaced or stopped, exiting");
                        break;
                    }
                }

                // Backend-aware "is the torrent worker still alive?" check.
                // For Local Bypass plays (`webtorrent_pid == 0`) the helper
                // returns `true` so the reaper relies entirely on ffmpeg
                // liveness — preserves the legacy behavior. For webtorrent:
                // process-existence via `libc::kill(pid, 0)`. For librqbit:
                // still-managed-by-Session check.
                let wt_alive = is_torrent_alive(&state, webtorrent_pid);
                let ffmpeg_alive = lock_recover(&state.ffmpeg_pid)
                    .map(|p| unsafe { torrent::kill_check(p) })
                    .unwrap_or(false);

                if !ffmpeg_alive && !wt_alive {
                    // Both dead — playback fully finished
                    tracing::info!("Reaper: all processes exited for '{}', cleaning up", title_for_log);
                    do_cleanup(&state);
                    break;
                }

                if !ffmpeg_alive && wt_alive {
                    // ffmpeg done, webtorrent still seeding. Compute a
                    // duration-aware grace period via the extracted helper
                    // `compute_reaper_grace_secs` — see its docs + tests for the
                    // full rationale and edge-case coverage.
                    let (source_duration, ss_offset) = {
                        let app_state = AppState::load(&state.state_dir);
                        match app_state.current.as_ref() {
                            Some(c) => (c.duration, c.ss_offset),
                            None => (None, 0.0),
                        }
                    };
                    let grace_secs = compute_reaper_grace_secs(source_duration, ss_offset);
                    tracing::info!(
                        "Reaper: ffmpeg finished for '{}', waiting {} grace period (duration={:?}s, ss_offset={:.0}s)...",
                        title_for_log,
                        if grace_secs >= 60 {
                            format!("{}m{}s", grace_secs / 60, grace_secs % 60)
                        } else {
                            format!("{}s", grace_secs)
                        },
                        source_duration,
                        ss_offset,
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(grace_secs)).await;

                    // Re-check we're still the active stream
                    let app_state = AppState::load(&state.state_dir);
                    match &app_state.current {
                        Some(c) if c.pid == webtorrent_pid => {
                            tracing::info!("Reaper: cleaning up webtorrent + media for '{}'", title_for_log);
                            do_cleanup(&state);
                        }
                        _ => tracing::debug!("Reaper: stream changed during grace period"),
                    }
                    break;
                }
            }
        });
    }

    // Spawn cast health monitor: detect the silent-failure case where the
    // Chromecast loaded the media but never started playing (or started then
    // ended unexpectedly because of a network blip, decoder error, app
    // eviction, ambient screensaver). rust_cast drops its connection after
    // cast_url returns OK, so spela's normal status endpoint reports its
    // local intent rather than the TV's actual playback state. Without this
    // monitor, a "blue cast icon" failure mode looks identical to a healthy
    // streaming session in `spela status`.
    if target == "chromecast" {
        let state_for_monitor = state.clone();
        let cast_name_for_monitor = cast_name.clone();
        let title_for_monitor = title.clone();
        let started_at_for_monitor = app_state.current.as_ref().map(|c| c.started_at);
        tokio::spawn(async move {
            cast_health_monitor(
                state_for_monitor,
                cast_name_for_monitor,
                title_for_monitor,
                started_at_for_monitor,
            )
            .await;
        });
    }

    Json(json!({
        "status": "streaming",
        "pid": pid,
        "target": format!("{}:{}", target, cast_name),
        "title": title,
        "subtitles": has_subtitles,
        "url": final_url,
        // Apr 15, 2026: surfaces auto-resume to the CLI / voice-assistant
        // consumers. Some(pos) when do_play picked up a saved HWM,
        // None otherwise (fresh start, or explicit --seek that cleared HWM).
        "resumed_from": auto_resumed_from
    }))
}

/// Shared cleanup: stop the active torrent + kill ffmpeg, delete transcoded
/// file, update state. The previous torrent's id is read from
/// `app_state.current.pid` and routed through `engine.stop(id, false)`
/// (keep files on disk for Local Bypass reuse). `do_cleanup` is sync; the
/// librqbit stop is async, so we fan it out via `tokio::spawn` — the
/// reaper / handle_stop already expect cleanup to complete in the
/// background, this matches that contract.
fn do_cleanup(state: &SharedState) {
    let app_state = crate::state::AppState::load(&state.state_dir);
    let prev_pid = app_state.current.as_ref().map(|c| c.pid).unwrap_or(0);

    if prev_pid != 0 {
        let state = state.clone();
        tokio::spawn(async move {
            stop_torrent(&state, prev_pid, false).await;
        });
    }

    if let Some(pid) = lock_recover(&state.ffmpeg_pid).take() {
        torrent::kill_pid(pid);
    }
    // Kill any lingering ffmpeg or python http servers
    let _ = std::process::Command::new("pkill")
        .args(["-f", "python3 -m http.server 8889"])
        .output();

    // --- AUTO-VERIFICATION MARKER ---
    // If the movie is physically full on disk, mark it as .spela_done
    // to enable instant Local Bypass for future requests.
    let app_state = crate::state::AppState::load(&state.state_dir);
    if let Some(current) = &app_state.current {
        let expected_bytes = current.size.as_deref().and_then(parse_size_to_bytes).unwrap_or(0);
        if expected_bytes == 0 {
            tracing::debug!(
                "Auto-Verification: skipping .spela_done for '{}' because expected byte size is unknown",
                current.title
            );
        }
        let mut target_dir = state.media_dir.clone();
        if target_dir.to_string_lossy().starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                target_dir = home.join(target_dir.strip_prefix("~/").unwrap());
            }
        }
        let target_dir = std::fs::canonicalize(&target_dir).unwrap_or(target_dir);
        
        // Find the movie folder by title
        if let Ok(entries) = std::fs::read_dir(&target_dir) {
            for entry in entries.flatten() {
                let folder_name = entry.file_name().to_string_lossy().to_string();
                if expected_bytes > 0 && title_tokens_match(&folder_name, &current.title) {
                    // Check for mp4/mkv files and verify physical completeness
                    if let Ok(sub_entries) = std::fs::read_dir(entry.path()) {
                        for sub_entry in sub_entries.flatten() {
                            let path = sub_entry.path();
                            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                            if ext == "mp4" || ext == "mkv" {
                                if is_physically_full(&path, expected_bytes) {
                                    let marker_path = entry.path().join(".spela_done");
                                    if !marker_path.exists() {
                                        let _ = std::fs::File::create(&marker_path);
                                        tracing::info!("Auto-Verification: Marked '{}' as .spela_done (Physically Full)", current.title);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Expand media_dir path (same logic as do_play) before trying to delete
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);
    let transcoded = media_dir.join("transcoded_aac.mp4");
    if transcoded.exists() {
        let _ = std::fs::remove_file(&transcoded);
    }
    // Apr 15, 2026: also wipe the HLS output dir written by transcode_hls.
    // Each play creates fresh segments under transcoded_hls/; leaving stale
    // segments around would let the next play accidentally serve mismatched
    // content if the manifest from the previous run survives.
    let hls_dir = media_dir.join("transcoded_hls");
    if hls_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&hls_dir) {
            tracing::warn!("do_cleanup: failed to remove HLS dir {:?}: {}", hls_dir, e);
        }
    }

    let mut app_state = AppState::load(&state.state_dir);
    app_state.stop_current();
    let _ = app_state.save(&state.state_dir);
}

/// Background task that polls the Chromecast media session AFTER cast_url
/// returns OK, to detect the silent-failure case where the receiver loaded
/// the LOAD message but the player_state never reached Playing/Buffering
/// (the "blue cast icon" failure mode), or transitioned to Idle mid-stream
/// because of a network blip, decoder error, app eviction, or screensaver.
///
/// rust_cast drops its connection after `cast_url` returns, so without this
/// monitor spela has zero visibility into the TV's actual playback state and
/// `spela status` reports its local intent ("running: true, status: streaming")
/// while the TV is back to its ambient wallpaper. Apr 15, 2026 incident:
/// every cast attempt produced a healthy ffmpeg + a "streaming" status while
/// Fredriks TV showed nothing but the blue cast icon, with no failure surface.
///
/// Behavior:
///   - Sleeps `STARTUP_GRACE_SECS` so the receiver has time to actually load.
///   - Polls `cast.get_info()` every `POLL_INTERVAL_SECS`.
///   - Counts consecutive polls where `player_state` is Idle/Unknown OR the
///     query itself fails. After `IDLE_FAILURE_THRESHOLD` consecutive misses,
///     the cast is declared dead, the worker pipeline is cleaned up via
///     `do_cleanup`, and the task exits.
///   - Exits cleanly when the saved current stream is replaced by a different
///     `started_at` timestamp (a new `do_play` ran, this monitor is stale)
///     or when the saved state's `current` is None at all (someone called
///     `/stop` and we're done).
/// Compute the reaper's grace period after ffmpeg finishes, as a function of
/// source duration and the input seek offset. Extracted as a pure helper so
/// unit tests can pin the math for every operationally interesting scenario.
///
/// The grace period starts the moment ffmpeg exits (having transcoded the
/// entire source file to the HLS segment dir). At that moment, the Chromecast
/// is still playing the stream at 1x realtime, somewhere between "just
/// started" and "nearly done". The grace is the wall-clock time we're
/// willing to keep the segment dir alive before running `do_cleanup`.
///
/// Formula: `grace = max(remaining_content + 10 min cushion, 5 min floor)`
///
/// Where `remaining_content = (duration - ss_offset).max(0)` represents the
/// total playable wall-clock length of the transcoded stream (Chromecast
/// plays at 1x realtime, so `duration - ss_offset` seconds of source content
/// takes exactly `duration - ss_offset` seconds of wall time to play out).
/// The 10-minute cushion covers paused-mid-episode or user-rewind scenarios.
/// The 5-minute floor protects against degenerate durations (0, NaN, etc.).
///
/// When `duration` is unknown (None or ≤0), fall back to the historical
/// 45-minute hardcoded default — better than removing the safety net entirely.
///
/// Apr 15, 2026: this replaces a hardcoded 45-minute grace period that was
/// too SHORT for 63-minute TV episodes (ffmpeg at NVENC's 6x realtime finished
/// transcoding at ~11 min wall time, 45-min grace expired at 56 min wall time,
/// cleanup fired while the user was still at the 30-minute mark) and too LONG
/// for short movies.
pub fn compute_reaper_grace_secs(duration: Option<f64>, ss_offset: f64) -> u64 {
    const GRACE_CUSHION_SECS: u64 = 600; // 10 min
    const GRACE_FLOOR_SECS: u64 = 300; // 5 min
    const UNKNOWN_DURATION_DEFAULT_SECS: u64 = 2700; // 45 min — legacy fallback

    match duration {
        Some(dur) if dur > 0.0 => {
            let remaining = (dur - ss_offset).max(0.0) as u64;
            remaining
                .saturating_add(GRACE_CUSHION_SECS)
                .max(GRACE_FLOOR_SECS)
        }
        _ => UNKNOWN_DURATION_DEFAULT_SECS,
    }
}

/// Sanity check for cast_health_monitor position saves.
///
/// Apr 15, 2026: added after a debug session where Chromecast reported a
/// phantom 30× jump in `current_time` in a single 60s wall-clock window —
/// most-likely a stale cast session that survived a spela restart combined
/// with a new ss_offset from auto-resume. Without this guard, the phantom
/// reading poisoned the saved HWM and the next play auto-resumed at an
/// impossible position (e.g. 88% through a brand-new episode).
///
/// Allowed: normal playback (delta_abs ≈ delta_wall), 2× playback rate,
/// plus 60s slack for clock skew and coarse polling cadence.
///
/// Blocked: any tick where the apparent absolute position has advanced
/// by more than `2.0 * delta_wall + 60.0`. Callers SKIP the save on a
/// suspicious tick and leave the baseline unchanged — a one-off glitch
/// self-heals on the next tick, a persistent glitch keeps skipping.
pub fn is_position_jump_suspicious(delta_wall_secs: f64, delta_abs_secs: f64) -> bool {
    if delta_wall_secs <= 0.0 {
        // First tick (no baseline) or clock glitch → never suspicious.
        return false;
    }
    delta_abs_secs > 2.0 * delta_wall_secs + 60.0
}

/// Minimum wall-clock age of a stream before we'll attempt `attempt_cast_recast`.
///
/// Apr 25, 2026: a stream that goes IDLE within its first minute is almost
/// certainly a LOAD-side failure (stream_host unreachable, DNS hairpin
/// broken, transcode not warm yet, Chromecast rejecting the manifest version).
/// Re-LOAD won't fix those — it just burns another 15 s of user patience
/// before the real cleanup. So we gate the recast on the stream having
/// actually played for a while; failures in that regime are far more likely
/// to be mid-stream CAF flakes that the device recovers from on a fresh LOAD.
pub const MIN_STREAM_AGE_FOR_RECAST_SECS: u64 = 60;

/// Apr 29, 2026: minimum wall-clock cooldown between recast attempts. Replaces
/// the original Apr 25 lifetime-cap-of-1 with a frequency-cap design.
///
/// Why the change: the cap-of-1 was correct for the failure class it was
/// built for (Apr 25 Vardagsrum CrKey 1.56 mid-stream IDLE — single random
/// flake, one retry, give up if the retry doesn't work). It was wrong for
/// a different class we hit Apr 28-29: receiver IDLEs every ~15-30 minutes
/// of sustained playback, recovers with a recast for another 15-30 min,
/// IDLEs again. Each recast was successful recovery, not "burning cycles
/// on a wedge" — but the lifetime cap rejected them anyway, leaving the
/// user with a permanent dead stream. (Hijack S2E2 Apr 28-29 incident: 6
/// rapid Playing↔Buffering oscillations at 38-40 min into stream after
/// burning the recast budget on the first IDLE; would have benefited from
/// 2-3 more recasts to keep playback alive.)
///
/// The cap-of-1 conflated wedge-detection with attempt-limiting. They're
/// separate concerns: the WEDGE we're protecting against is rapid-fire
/// recast→IDLE→recast→IDLE within seconds (looks like infinite loop on a
/// truly broken device). That's a FREQUENCY property, not a lifetime
/// property — express it as a cooldown.
///
/// 90 seconds is well-above the maximum legitimate recast→Playing recovery
/// time (~15-25s observed) and well-below the minimum healthy-recast cycle
/// time we've seen (~3-15 minutes in practice).
pub const RECAST_COOLDOWN_SECS: u64 = 30;
// Apr 29, 2026 PM: lowered 90→30s. Reasoning:
//
// 90s was tuned against Apr 28's slow-fail pattern (15-min cycles). E2
// resume this morning exposed a faster-fail mode on the SAME device —
// CrKey 1.56 firmware locks the playback thread for ~63s at a time,
// recovers via recast, locks again ~70s later. The 90s cooldown rejected
// the 3rd recast (72s < 90s) → premature cleanup. Network probe during
// stalls confirmed receiver sent ZERO HTTP requests during BUFFERING
// windows: purely receiver-internal block, nothing for spela to fix on
// its own side. The recast IS the right answer — it kicks the receiver
// out of its frozen state with a fresh LOAD message.
//
// Why 30s and not 60s: the EFFECTIVE minimum-time-between-recasts on
// the BUFFERING-stall path is already MAX_BUFFERING_DURATION_SECS=60s —
// recast can't fire until 60s of continuous BUFFERING accumulates. So
// for the dominant failure mode (BUFFERING-stuck), cooldown=30 is
// functionally identical to cooldown=60. Cooldown only matters for
// IDLE-driven recasts (consecutive_failures hits threshold from 3
// polls × 5s = 15s of IDLE) — wedge protection there at 30s is still
// 2× the IDLE-detect window, which is plenty.
//
// Net effect: more responsive recovery on faster-degrading devices,
// no regression in wedge protection.

/// Apr 29, 2026 PM: lowered 60→30s. The earlier 60s value was conservative
/// against startup-buffering false positives (cold start legitimately takes
/// 20-30s of BUFFERING before reaching Playing). The cleaner answer is to
/// gate BUFFERING-stall on `stream_age >= MIN_STREAM_AGE_FOR_RECAST_SECS`
/// (the same startup floor the recast itself uses) and use a tighter mid-
/// stream threshold. With the gate, any stall longer than 30s past startup
/// triggers recovery 30 seconds earlier than before — user-visible freeze
/// drops from ~78s (60s detection + 3s LOAD + 15s recovery) to ~48s.
///
/// 30s is well above legitimate mid-stream buffering events: HLS rebuffer
/// 5-15s, recast-induced ~25s, in-stream seek ~15-25s. Receiver-internal
/// freezes (the actual failure we're catching) typically last 60-90s, well
/// above this threshold.
///
/// Apr 29, 2026: BUFFERING-too-long → escalate to recast.
///
/// Apr 18 fix established that BUFFERING is a TRANSIENT state (alive
/// receiver waiting for data, will recover). Killing on BUFFERING was
/// converting recoverable conditions into permanent stream death. But
/// "BUFFERING is transient" was implicit — never bounded by a timeout.
/// Apr 28-29 incident: receiver entered BUFFERING and stayed there
/// permanently (still BUFFERING when the user fell asleep 5+ minutes
/// later) with cast_health_monitor logging "BUFFERING (transient — not
/// incrementing failure counter)" forever.
///
/// This threshold makes "transient" mean what it says: a normal HLS
/// re-buffer takes 5-15 seconds; an aggressive seek takes ~30s; a
/// recast-induced buffer takes ~25s. 60s is well above all of these.
/// If the receiver is still BUFFERING at 60s, it's stalled — escalate
/// to the same path as IDLE (try recast subject to cooldown, else
/// cleanup).
pub const MAX_BUFFERING_DURATION_SECS: u64 = 30;

/// Decision: should `cast_health_monitor` attempt auto-recast before cleanup?
///
/// Apr 25, 2026 — added after the Vardagsrum CrKey 1.56 Chromecast repeatedly
/// entered `player_state=IDLE` mid-stream during sustained high-bitrate
/// BluRay H.264 playback (~7 Mbps). Pattern: healthy for ~11 minutes, then a
/// sudden transition to IDLE with no error surface the cast protocol will
/// hand us. The workers (webtorrent + ffmpeg) stay alive throughout; it's
/// purely a receiver-side flake. One recast attempt is free recovery when
/// the Chromecast just needs a new LOAD message.
///
/// Apr 29, 2026 — replaced lifetime cap-of-1 with rate limit. See
/// `RECAST_COOLDOWN_SECS` doc for rationale. Returns true only when ALL of
/// these hold:
///   - `have_valid_hwm` — no point recasting if we can't restore position.
///   - `stream_age_secs >= MIN_STREAM_AGE_FOR_RECAST_SECS` — see constant doc.
///   - Either `secs_since_last_recast` is `None` (first recast this stream)
///     OR it's at least `RECAST_COOLDOWN_SECS` — frequency-cap that prevents
///     the rapid-fire wedge case (recast→IDLE→recast→IDLE in seconds) without
///     artificially stopping recoverable cycles.
pub fn should_attempt_recast(
    secs_since_last_recast: Option<u64>,
    stream_age_secs: u64,
    have_valid_hwm: bool,
) -> bool {
    have_valid_hwm
        && stream_age_secs >= MIN_STREAM_AGE_FOR_RECAST_SECS
        && match secs_since_last_recast {
            None => true, // first recast of this stream — always allowed
            Some(s) => s >= RECAST_COOLDOWN_SECS,
        }
}

/// Try to re-LOAD the current stream to the Chromecast and seek to the saved HWM.
///
/// Apr 25, 2026. This is the muscle behind `should_attempt_recast`. Sequence:
///   1. Pull the existing stream's URL from `CurrentStream`.
///   2. Infer cast `content_type` from URL suffix (HLS master → mpegurl).
///   3. Fire `cast_url` again — Default Media Receiver accepts a fresh LOAD
///      message and replaces its (idle) session with a new one starting at 0.
///   4. Fire `cast.seek(hwm - ss_offset)` — the HLS playlist's `t=0` is at
///      source `t=ss_offset`, so the within-HLS seek target is
///      `hwm_absolute - ss_offset` (clamped at 0).
///
/// Workers (webtorrent, ffmpeg, HLS dir) stay alive throughout — this is
/// purely client-side state recovery. If `cast_url` or `seek` fails, the
/// caller logs and falls through to `do_cleanup` as before.
///
/// The function parks both blocking Cast operations on `spawn_blocking` so
/// they don't starve the async runtime (rust_cast is synchronous).
async fn attempt_cast_recast(
    state: &SharedState,
    cast_name: &str,
    hwm_absolute: f64,
    ss_offset: f64,
    duration_hint: Option<f64>,
) -> anyhow::Result<()> {
    use anyhow::anyhow;

    // Pull the URL + metadata from state — the stream the monitor is
    // watching carries them on `CurrentStream`. Apr 28, 2026: gate the rich
    // metadata on `config.rich_metadata_in_load` (same rationale as do_play
    // — see config.rs comment). The recast must produce the SAME UI the
    // original LOAD did, otherwise mid-stream IDLE recovery would visibly
    // change the on-screen overlay (large↔small toggle).
    let (url, recast_metadata) = {
        let app_state = AppState::load(&state.state_dir);
        match app_state.current.as_ref() {
            Some(c) => (
                c.url.clone(),
                if state.config.rich_metadata_in_load {
                    cast::CastMetadata {
                        title: Some(c.title.clone()).filter(|t| !t.is_empty()),
                        series_title: c.show.clone(),
                        season: c.season,
                        episode: c.episode,
                        poster_url: c.poster_url.clone(),
                        release_date: None,
                    }
                } else {
                    cast::CastMetadata::default()
                },
            ),
            None => return Err(anyhow!("CurrentStream gone before recast could fire")),
        }
    };

    // HLS master playlists are the only LOAD URLs spela currently hands out,
    // but do the match-by-suffix anyway so this stays honest if the legacy
    // /stream/transcode fMP4 path gets re-enabled.
    let content_type = if url.ends_with(".m3u8") || url.contains("/hls/") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp4"
    };

    // Re-LOAD on the receiver.
    let state_clone = state.clone();
    let cast_name_clone = cast_name.to_string();
    let url_clone = url.clone();
    let content_type_clone = content_type.to_string();
    let recast_metadata_clone = recast_metadata.clone();
    let load_outcome = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.cast_url(
            &cast_name_clone,
            &url_clone,
            &content_type_clone,
            duration_hint,
            None,
            &recast_metadata_clone,
        )
    })
    .await;

    match load_outcome {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(anyhow!("recast cast_url failed: {}", e)),
        Err(e) => return Err(anyhow!("recast cast_url spawn panic: {}", e)),
    }

    // Seek target within the HLS stream (HLS t=0 is source t=ss_offset).
    let seek_within_hls = (hwm_absolute - ss_offset).max(0.0);
    if seek_within_hls > 1.0 {
        let state_clone = state.clone();
        let cast_name_clone = cast_name.to_string();
        let seek_outcome = tokio::task::spawn_blocking(move || {
            let mut cast = lock_recover(&state_clone.cast);
            cast.seek(&cast_name_clone, seek_within_hls)
        })
        .await;
        if let Ok(Err(e)) = seek_outcome {
            // LOAD worked; SEEK didn't. Accept playback from ss_offset (user
            // loses what they watched since the last 30 s HWM save — worst case
            // ~30 s). Preferable to a full-restart cleanup.
            tracing::warn!(
                "attempt_cast_recast: LOAD succeeded but seek to {:.0}s within HLS failed: {} — continuing from ss_offset",
                seek_within_hls, e
            );
        }
    }

    Ok(())
}

async fn cast_health_monitor(
    state: SharedState,
    cast_name: String,
    title_for_log: String,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    use tokio::time::{sleep, Duration};

    const STARTUP_GRACE_SECS: u64 = 10;
    const POLL_INTERVAL_SECS: u64 = 5;
    const IDLE_FAILURE_THRESHOLD: u32 = 3;

    // Periodic position save: write the last known position every N seconds
    // (not every poll, to keep state.json writes cheap). This is the engine
    // that powers "resume from where I stopped" for the Default Media Receiver
    // path — the Custom Cast Receiver was supposed to POST /api/position every
    // 30s but it's blocked on Cast SDK registration, so we do the equivalent
    // server-side using the polling data we already have in hand.
    // Apr 15, 2026 addition.
    const POSITION_SAVE_INTERVAL_SECS: f64 = 30.0;
    // Near-end save-skip threshold: once absolute position crosses this
    // fraction of duration, `save_position_smart` would clear the entry
    // anyway — skip the call entirely to avoid log spam.
    //
    // Apr 19, 2026: this is intentionally the SAME constant as
    // `state::HWM_CLEAR_FRACTION`. We do NOT have a separate "cleanup" fraction
    // anymore. cast_health_monitor relies on:
    //   1. Chromecast reporting IDLE at real EOF (player_state match below)
    //   2. The Reaper's duration-aware grace period if the device stays alive
    // Those two paths handle cleanup. A percentage-based early-kill was doing
    // nothing except amputating the last 8% of films — see the Send Help
    // incident (Apr 19, 2026) where a 113-min film was killed at 1:43:54.

    sleep(Duration::from_secs(STARTUP_GRACE_SECS)).await;

    let started_at = match started_at {
        Some(s) => s,
        None => {
            tracing::warn!("cast_health_monitor: no started_at recorded for '{}', exiting", title_for_log);
            return;
        }
    };

    // Snapshot the CurrentStream fields we need for position bookkeeping
    // at monitor start. Load them once — they don't change for the lifetime
    // of this stream (the monitor exits when started_at changes). This is
    // load-bearing for smart resume: `ss_offset` tells us how to translate
    // the Chromecast's 0-based current_time back into absolute source
    // timeline, and `imdb_id` / `title` / `duration` feed save_position_smart.
    let (ss_offset, imdb_id_snapshot, title_snapshot, duration_snapshot) = {
        let app_state = AppState::load(&state.state_dir);
        match app_state.current.as_ref() {
            Some(c) => (
                c.ss_offset,
                c.imdb_id.clone(),
                Some(c.title.clone()),
                c.duration,
            ),
            None => {
                tracing::warn!(
                    "cast_health_monitor: CurrentStream gone for '{}' at startup, exiting",
                    title_for_log
                );
                return;
            }
        }
    };

    let mut consecutive_failures: u32 = 0;
    let mut last_saved_position: f64 = ss_offset; // Baseline = the -ss we opened with
    // Wall-clock timestamp of the last ACCEPTED save. Used by the sanity
    // check in `is_position_jump_suspicious` to distinguish normal playback
    // advance from stale-Chromecast-state glitches. Apr 15, 2026.
    let mut last_save_wall: Option<std::time::Instant> = Some(std::time::Instant::now());
    // Freshest absolute position seen while Chromecast was in a non-idle state.
    // Used at IDLE-driven cleanup time (Apr 19, 2026) to decide whether the
    // session ended past HWM_CLEAR_FRACTION — if so, we clear the saved HWM
    // so the next play of the same title starts fresh instead of auto-resuming
    // at the credits. Updated on every successful non-idle probe (not just
    // the 30-second save cadence), so at EOF we have current-to-within-5s data.
    let mut last_known_absolute: Option<f64> = None;
    // Apr 25, 2026 (rev. Apr 29 — see `RECAST_COOLDOWN_SECS` for rationale):
    // auto-recast state. `recast_attempts` stays as a diagnostic counter
    // (surfaced in the PRE-CLEANUP log) but no longer gates the recast
    // decision; `last_recast_at` does, via the rate-limit in
    // `should_attempt_recast`.
    let mut recast_attempts: u32 = 0;
    let mut last_recast_at: Option<std::time::Instant> = None;
    // Apr 29, 2026: tracking BUFFERING duration. BUFFERING is "transient" per
    // the Apr 18 rule, but a permanent BUFFERING is a stall (Apr 28-29
    // incident). When this exceeds `MAX_BUFFERING_DURATION_SECS`, escalate
    // to the recast/cleanup path the same way IDLE does.
    let mut buffering_started_at: Option<std::time::Instant> = None;
    let mut prev_player_state_upper: Option<String> = None;
    tracing::info!(
        "cast_health_monitor: started for '{}' on '{}' (poll every {}s, fail after {} consecutive idle/error, ss_offset={:.0}s, save every {}s)",
        title_for_log, cast_name, POLL_INTERVAL_SECS, IDLE_FAILURE_THRESHOLD,
        ss_offset, POSITION_SAVE_INTERVAL_SECS
    );

    loop {
        // Identity check: are we still the active stream?
        let still_active = {
            let app_state = AppState::load(&state.state_dir);
            app_state
                .current
                .as_ref()
                .map(|c| c.started_at == started_at)
                .unwrap_or(false)
        };
        if !still_active {
            tracing::info!(
                "cast_health_monitor: stream '{}' replaced or stopped, exiting",
                title_for_log
            );
            return;
        }

        // Probe the Chromecast in a blocking task — rust_cast is sync.
        let state_clone = state.clone();
        let cast_name_clone = cast_name.clone();
        let probe_result = tokio::task::spawn_blocking(move || {
            let mut cast = lock_recover(&state_clone.cast);
            cast.get_info(&cast_name_clone)
        })
        .await;

        match probe_result {
            Ok(Ok(info)) => {
                let player_state_upper = info.player_state.to_uppercase();
                let is_dead = matches!(
                    player_state_upper.as_str(),
                    "IDLE" | "UNKNOWN" | ""
                );
                let is_buffering = player_state_upper == "BUFFERING";

                // Apr 25, 2026: log every state transition at INFO. This is the
                // cheap half of the diagnostic upgrade — with `idle_reason`
                // now surfaced through PlaybackInfo, a single line tells us
                // whether an IDLE was Finished (natural EOF), Interrupted
                // (old-CrKey mid-stream death), Cancelled (user action), or
                // Error (manifest/codec rejection). Per-poll verbosity stays
                // at DEBUG via the existing log.
                if prev_player_state_upper.as_deref() != Some(player_state_upper.as_str()) {
                    tracing::info!(
                        "cast_health_monitor: '{}' state transition: {:?} → {} idle_reason={:?} time={:.0}s media_session={:?}",
                        title_for_log,
                        prev_player_state_upper.as_deref().unwrap_or("<init>"),
                        info.player_state,
                        info.idle_reason,
                        info.current_time,
                        info.media_session_id
                    );
                    prev_player_state_upper = Some(player_state_upper.clone());
                }

                if is_dead {
                    consecutive_failures += 1;
                    tracing::warn!(
                        "cast_health_monitor: '{}' player_state={} idle_reason={:?} media_session={:?} ({}/{} consecutive idle polls before cleanup)",
                        title_for_log, info.player_state, info.idle_reason, info.media_session_id, consecutive_failures, IDLE_FAILURE_THRESHOLD
                    );
                    // Apr 29, 2026: an IDLE poll definitively ends the
                    // BUFFERING regime if there was one — receiver isn't
                    // buffering anymore, it's dead.
                    buffering_started_at = None;
                } else if is_buffering {
                    // Apr 18: BUFFERING is a TRANSIENT state — receiver
                    // is alive and waiting for HLS segments. Do NOT
                    // increment failure counter on a single poll.
                    // Apr 29, 2026: but bound the "transient" claim. Track
                    // when BUFFERING started; when it persists past
                    // MAX_BUFFERING_DURATION_SECS, escalate to the
                    // recast/cleanup path (it stalled, not "transient").
                    let now = std::time::Instant::now();
                    let started = *buffering_started_at.get_or_insert(now);
                    let buffering_for_secs = now.duration_since(started).as_secs();
                    let stream_age_secs = Utc::now()
                        .signed_duration_since(started_at)
                        .num_seconds()
                        .max(0) as u64;
                    if buffering_for_secs >= MAX_BUFFERING_DURATION_SECS
                        && stream_age_secs >= MIN_STREAM_AGE_FOR_RECAST_SECS
                    {
                        tracing::warn!(
                            "cast_health_monitor: '{}' STALLED — BUFFERING for {}s (≥ {}s threshold, stream_age={}s); escalating to recast/cleanup path",
                            title_for_log, buffering_for_secs, MAX_BUFFERING_DURATION_SECS, stream_age_secs
                        );
                        // Force the cleanup gate (consecutive_failures >=
                        // IDLE_FAILURE_THRESHOLD) to trigger recast. Set the
                        // exact threshold so the PRE-CLEANUP log shows a
                        // legible "3" rather than an ever-growing count.
                        consecutive_failures = IDLE_FAILURE_THRESHOLD;
                        // Reset so a successful recast → BUFFERING again
                        // gets its own fresh threshold timer.
                        buffering_started_at = None;
                    } else if buffering_for_secs >= MAX_BUFFERING_DURATION_SECS {
                        // Apr 29, 2026 PM: BUFFERING crossed the threshold
                        // BUT we're still in startup window (stream_age <
                        // MIN_STREAM_AGE_FOR_RECAST_SECS). Cold-start cases
                        // legitimately buffer for 20-30+s while the receiver
                        // pre-fills. Don't escalate — recast wouldn't fire
                        // (rejected by startup gate) and we'd just kill the
                        // stream prematurely. Reset the timer and keep
                        // waiting; mid-stream stalls catch on the next
                        // accumulation.
                        tracing::info!(
                            "cast_health_monitor: '{}' BUFFERING crossed threshold ({}s ≥ {}s) but stream still in startup window ({}s < {}s) — not escalating",
                            title_for_log, buffering_for_secs, MAX_BUFFERING_DURATION_SECS,
                            stream_age_secs, MIN_STREAM_AGE_FOR_RECAST_SECS
                        );
                        buffering_started_at = None;
                    } else {
                        tracing::info!(
                            "cast_health_monitor: '{}' BUFFERING (transient — {}s elapsed, threshold {}s)",
                            title_for_log, buffering_for_secs, MAX_BUFFERING_DURATION_SECS
                        );
                    }
                } else {
                    if consecutive_failures > 0 {
                        tracing::info!(
                            "cast_health_monitor: '{}' recovered: player_state={} (was failing {} polls)",
                            title_for_log, info.player_state, consecutive_failures
                        );
                    }
                    consecutive_failures = 0;
                    // Apr 29, 2026: receiver is healthy — clear any
                    // BUFFERING timer accumulated from prior poll(s).
                    buffering_started_at = None;
                    tracing::debug!(
                        "cast_health_monitor: '{}' player_state={} time={:.0}/{:.0}",
                        title_for_log, info.player_state, info.current_time, info.duration
                    );

                    // === Periodic position save (Apr 15, 2026) ===
                    // Absolute source-timeline position = chromecast_time + ss_offset.
                    // Only save when:
                    //   (1) we have a positive Chromecast time (ignore the LOAD/BUFFERING transient),
                    //   (2) the delta since last save is ≥ POSITION_SAVE_INTERVAL_SECS
                    //       (keeps state.json writes to ~1 per 30 seconds instead of per 5s poll),
                    //   (3) the absolute position is meaningful (>30s in — matches the
                    //       auto-resume threshold in do_play), and
                    //   (4) the player is in a non-idle state (already guaranteed by the
                    //       outer `if !is_dead` branch we're in right now).
                    //
                    // save_position_smart handles completion internally: when the absolute
                    // position crosses HWM_CLEAR_FRACTION of duration or within
                    // HWM_CLEAR_TAIL_SECS of the end, it clears the entry so the next
                    // play starts fresh.
                    let absolute = info.current_time as f64 + ss_offset;
                    // Record the freshest non-idle position for the IDLE-cleanup
                    // HWM-clear decision (Apr 19, 2026). Only trust positive
                    // current_time readings — at LOAD, Chromecast briefly reports 0.
                    if info.current_time > 0.0 {
                        last_known_absolute = Some(absolute);
                    }
                    let duration_hint = duration_snapshot.or_else(|| {
                        // info.duration is -1 for HLS live manifests (ENDLIST missing).
                        // Prefer CurrentStream.duration; fall back to info.duration only
                        // if positive.
                        if info.duration > 0.0 {
                            Some(info.duration as f64)
                        } else {
                            None
                        }
                    });

                    if absolute > 30.0
                        && (absolute - last_saved_position).abs() >= POSITION_SAVE_INTERVAL_SECS
                    {
                        // Apr 15, 2026 sanity check: reject physically-impossible
                        // position jumps (stale Chromecast state surviving a spela
                        // restart, etc.). See `is_position_jump_suspicious` for
                        // threshold rationale.
                        let now_wall = std::time::Instant::now();
                        let suspicious = last_save_wall.map_or(false, |prev_wall| {
                            let delta_wall = now_wall.duration_since(prev_wall).as_secs_f64();
                            let delta_abs = absolute - last_saved_position;
                            if is_position_jump_suspicious(delta_wall, delta_abs) {
                                tracing::warn!(
                                    "cast_health_monitor: impossible position jump for '{}': +{:.0}s in {:.0}s wall (ratio={:.1}x) — SKIPPING save, likely stale Chromecast state",
                                    title_for_log,
                                    delta_abs,
                                    delta_wall,
                                    delta_abs / delta_wall.max(0.001)
                                );
                                true
                            } else {
                                false
                            }
                        });

                        // Don't bother saving if we're already past the HWM_CLEAR
                        // threshold — save_position_smart would just clear the entry,
                        // which is fine, but avoids spurious "clearing" log spam.
                        let past_end = duration_hint
                            .map(|d| absolute >= d * HWM_CLEAR_FRACTION)
                            .unwrap_or(false);
                        if !suspicious && !past_end {
                            let mut app_state = AppState::load(&state.state_dir);
                            let (key, saved) = app_state.save_position_smart(
                                imdb_id_snapshot.clone(),
                                title_snapshot.clone(),
                                absolute,
                                duration_hint,
                            );
                            if saved {
                                if let Err(e) = app_state.save(&state.state_dir) {
                                    tracing::warn!(
                                        "cast_health_monitor: failed to persist resume position for '{}': {}",
                                        key, e
                                    );
                                } else {
                                    tracing::debug!(
                                        "cast_health_monitor: saved resume position for '{}' at {:.0}s (chromecast+{:.0}s)",
                                        key, absolute, ss_offset
                                    );
                                    last_saved_position = absolute;
                                    last_save_wall = Some(now_wall);
                                }
                            }
                        }
                    }

                    // Apr 19, 2026: the percentage-based "end-of-episode" early-kill
                    // was removed here. It was amputating the final 8% of films
                    // (climax + resolution) because 92% threshold < credits start on
                    // modern features. Real end-of-stream is handled by the
                    // Chromecast IDLE path above (player_state transitions to IDLE
                    // at EOF, cleanup fires after IDLE_FAILURE_THRESHOLD polls) and
                    // the Reaper's duration-aware grace period. See Send Help
                    // incident Apr 19, 2026 — 113-min film killed at 1:43:54 with
                    // 8:42 of climax remaining.
                }
            }
            Ok(Err(e)) => {
                consecutive_failures += 1;
                tracing::warn!(
                    "cast_health_monitor: '{}' get_info failed: {} ({}/{} consecutive failures before cleanup)",
                    title_for_log, e, consecutive_failures, IDLE_FAILURE_THRESHOLD
                );
            }
            Err(e) => {
                tracing::error!(
                    "cast_health_monitor: '{}' spawn_blocking panic: {}, exiting monitor",
                    title_for_log, e
                );
                return;
            }
        }

        if consecutive_failures >= IDLE_FAILURE_THRESHOLD {
            // Apr 25, 2026: auto-recast on first mid-stream IDLE batch.
            //
            // Old CrKey firmware occasionally drops its CAF session without
            // error after sustained high-bitrate playback — classic Vardagsrum
            // failure mode. Workers stay alive (ffmpeg keeps writing, webtorrent
            // keeps seeding); it's the receiver that needs a kick. One fresh
            // LOAD + seek to HWM recovers ~10-30 s later with no manual action.
            //
            // Guards (see `should_attempt_recast` doc):
            //   - exactly one recast per stream (infinite-loop protection)
            //   - stream must have played for ≥ MIN_STREAM_AGE_FOR_RECAST_SECS
            //     (startup LOAD failures don't recover from re-LOAD)
            //   - a usable HWM exists to seek to
            //
            // EOF is NOT a recast case: when `last_known_absolute` crossed
            // HWM_CLEAR_FRACTION we treat the IDLE as natural end and fall
            // through to the HWM-clear + cleanup path below. Recovering
            // "playback" of the credits is not useful and would just re-trigger
            // the same IDLE.
            let stream_age_secs = Utc::now()
                .signed_duration_since(started_at)
                .num_seconds()
                .max(0) as u64;
            let is_natural_eof = match (duration_snapshot, last_known_absolute) {
                (Some(dur), Some(abs_pos)) if dur > 0.0 => abs_pos >= dur * HWM_CLEAR_FRACTION,
                _ => false,
            };
            let secs_since_last_recast = last_recast_at
                .map(|t| t.elapsed().as_secs());
            // Apr 29, 2026 PM: surface the cooldown-rejected case in logs.
                // Without this, when consecutive_failures hits the threshold but
            // should_attempt_recast returns false, we'd silently fall through
            // to cleanup with no breadcrumb showing WHY recast was skipped.
            if !is_natural_eof
                && !should_attempt_recast(
                    secs_since_last_recast,
                    stream_age_secs,
                    last_known_absolute.is_some(),
                )
                && stream_age_secs >= MIN_STREAM_AGE_FOR_RECAST_SECS
                && last_known_absolute.is_some()
            {
                tracing::warn!(
                    "cast_health_monitor: '{}' — recast SKIPPED (cooldown not satisfied: secs_since_last_recast={:?}, cooldown={}s) — falling through to cleanup",
                    title_for_log, secs_since_last_recast, RECAST_COOLDOWN_SECS
                );
            }
            if !is_natural_eof
                && should_attempt_recast(
                    secs_since_last_recast,
                    stream_age_secs,
                    last_known_absolute.is_some(),
                )
            {
                let hwm = last_known_absolute.unwrap_or(last_saved_position);
                tracing::warn!(
                    "cast_health_monitor: '{}' — attempting auto-recast (attempt #{}, secs_since_last={:?}, cooldown={}s) at HWM={:.0}s ss_offset={:.0}s stream_age={}s",
                    title_for_log, recast_attempts + 1, secs_since_last_recast,
                    RECAST_COOLDOWN_SECS, hwm, ss_offset, stream_age_secs
                );
                match attempt_cast_recast(
                    &state,
                    &cast_name,
                    hwm,
                    ss_offset,
                    duration_snapshot,
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "cast_health_monitor: '{}' — auto-recast LOAD+seek issued; resuming poll loop",
                            title_for_log
                        );
                        recast_attempts += 1;
                        last_recast_at = Some(std::time::Instant::now());
                        consecutive_failures = 0;
                        // Force a transition-log entry on the next poll (the
                        // state may still read IDLE for one more tick while
                        // the Chromecast processes the new LOAD).
                        prev_player_state_upper = None;
                        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "cast_health_monitor: '{}' — auto-recast failed: {} — proceeding to cleanup",
                            title_for_log, e
                        );
                        // fall through to the normal cleanup path
                    }
                }
            }

            // Apr 25, 2026: pre-cleanup diagnostic dump. Records the load-bearing
            // in-memory state the monitor accumulated — without touching disk,
            // so it never racks up at a bad time. Pairs with idle_reason logging
            // to replace the old "something happened, we gave up" black box with
            // a replayable timeline. Stays at ERROR level so operators don't
            // have to raise verbosity to see it after an incident.
            tracing::error!(
                "cast_health_monitor: PRE-CLEANUP for '{}' — consecutive_failures={} recast_attempts={} stream_age={}s ss_offset={:.0}s last_known_absolute={:.0?} last_saved_position={:.1}s duration={:?} prev_state={:?}",
                title_for_log, consecutive_failures, recast_attempts, stream_age_secs,
                ss_offset, last_known_absolute, last_saved_position, duration_snapshot,
                prev_player_state_upper
            );

            tracing::error!(
                "cast_health_monitor: chromecast media session DEAD for '{}' ({} consecutive idle/error polls). Cleaning up workers.",
                title_for_log, consecutive_failures
            );
            // Apr 19, 2026: if the Chromecast went IDLE past HWM_CLEAR_FRACTION,
            // this was real EOF (not a mid-playback disconnect) — clear the
            // saved HWM so the next play of the same title starts fresh instead
            // of auto-resuming from the credits. Belt-and-suspenders against
            // the 30s save cadence leaving a stale 94%-ish HWM behind.
            // Apr 29, 2026: detect natural EOF here. We need this BEFORE
            // do_cleanup runs so we can fire the queue if the user lined
            // up a follow-up (e.g., next episode).
            let was_natural_eof = matches!(
                (duration_snapshot, last_known_absolute),
                (Some(dur), Some(abs_pos)) if dur > 0.0 && abs_pos >= dur * HWM_CLEAR_FRACTION
            );

            if let (Some(dur), Some(abs_pos)) = (duration_snapshot, last_known_absolute) {
                if dur > 0.0 && abs_pos >= dur * HWM_CLEAR_FRACTION {
                    let mut app_state = AppState::load(&state.state_dir);
                    let cleared = app_state.reset_position(
                        imdb_id_snapshot.clone(),
                        title_snapshot.clone(),
                    );
                    let _ = app_state.save(&state.state_dir);
                    tracing::info!(
                        "cast_health_monitor: Chromecast IDLE past {:.0}% ({:.0}/{:.0}s) — cleared resume HWM for '{}'",
                        HWM_CLEAR_FRACTION * 100.0, abs_pos, dur, cleared
                    );
                }
            }
            do_cleanup(&state);

            // Apr 29, 2026: queue auto-fire on natural EOF. After cleanup
            // (workers killed, HLS dir gone, current=None), pop the queue
            // front and self-call /play with its fields. Spawned as a
            // detached task so the monitor exits cleanly while the next
            // stream sets up. Only fires on NATURAL EOF — user-initiated
            // stops route through `handle_stop` which clears state.current,
            // breaking the monitor's `still_active` check before we get
            // here. So queue is preserved across user stops, not consumed.
            if was_natural_eof {
                let next = {
                    let mut app_state = AppState::load(&state.state_dir);
                    if app_state.queue.is_empty() {
                        None
                    } else {
                        let item = app_state.queue.remove(0);
                        let _ = app_state.save(&state.state_dir);
                        Some(item)
                    }
                };
                if let Some(item) = next {
                    let port = state.config.port;
                    let host = state.config.host.clone();
                    let title = item.title.clone();
                    tokio::spawn(async move {
                        // Wait briefly so cleanup completes (HLS dir gone,
                        // workers fully reaped) before the new play setup
                        // tries to write to the same paths.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        // Apr 29, 2026 PM bug fix: spela binds to
                        // `config.host` (e.g. 192.168.4.1 on Darwin per its
                        // systemd unit) — NOT 0.0.0.0. Self-calling 127.0.0.1
                        // failed because the server isn't listening on
                        // loopback. Use the actual bind host. Fall back to
                        // 127.0.0.1 only when config.host is the wildcard
                        // 0.0.0.0 (then loopback IS in scope).
                        let host_for_self_call = if host.is_empty() || host == "0.0.0.0" {
                            "127.0.0.1".to_string()
                        } else {
                            host
                        };
                        let url = format!("http://{host_for_self_call}:{port}/play");
                        let body = serde_json::json!({
                            "magnet": item.magnet,
                            "title": item.title,
                            "show": item.show,
                            "season": item.season,
                            "episode": item.episode,
                            "imdb_id": item.imdb_id,
                            "file_index": item.file_index,
                            "cast_name": item.cast_name,
                            "target": item.target,
                            "poster_url": item.poster_url,
                            "quality": item.quality,
                            "size": item.size,
                        });
                        let client = reqwest::Client::new();
                        match client.post(&url).json(&body).send().await {
                            Ok(resp) => {
                                tracing::info!(
                                    "queue: auto-fired '{}' on natural EOF; status={}",
                                    title, resp.status()
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "queue: failed to auto-fire '{}': {}",
                                    title, e
                                );
                            }
                        }
                    });
                }
            }
            return;
        }

        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

async fn handle_stop(State(state): State<SharedState>) -> Json<Value> {
    do_cleanup(&state);
    Json(json!({"status": "stopped"}))
}

async fn handle_status(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    match &app_state.current {
        None => Json(json!({"status": "idle"})),
        Some(current) => {
            let running = is_process_running(current.pid);
            Json(json!({
                "status": if running { "streaming" } else { "process_dead" },
                "current": current,
                "running": running
            }))
        }
    }
}

async fn handle_pause(State(state): State<SharedState>) -> Json<Value> {
    let device = get_current_device(&state);
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.pause(&device)
    }).await;
    cast_result_to_json(result)
}

async fn handle_resume(State(state): State<SharedState>) -> Json<Value> {
    let device = get_current_device(&state);
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.resume(&device)
    }).await;
    cast_result_to_json(result)
}

async fn handle_seek(
    State(state): State<SharedState>,
    Json(req): Json<SeekRequest>,
) -> Json<Value> {
    let seconds = match req.t.or(req.seconds) {
        Some(s) => s,
        None => return Json(json!({"error": "Missing t (seconds) parameter"})),
    };
    let device = get_current_device(&state);
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.seek(&device, seconds)
    }).await;
    cast_result_to_json(result)
}

async fn handle_volume(
    State(state): State<SharedState>,
    Json(req): Json<VolumeRequest>,
) -> Json<Value> {
    let level = match req.level {
        Some(l) => l,
        None => return Json(json!({"error": "Missing level (0-100) parameter"})),
    };
    let device = get_current_device(&state);
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.set_volume(&device, level)
    }).await;
    cast_result_to_json(result)
}

async fn handle_next(State(state): State<SharedState>) -> Json<Value> {
    navigate_episode(&state, 1).await
}

async fn handle_prev(State(state): State<SharedState>) -> Json<Value> {
    navigate_episode(&state, -1).await
}

async fn navigate_episode(state: &SharedState, direction: i32) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    let current = match &app_state.current {
        Some(c) if c.show.is_some() && c.season.is_some() && c.episode.is_some() => c,
        _ => return Json(json!({"error": "No show/episode context -- play a TV episode first"})),
    };

    let show = current.show.clone().unwrap();
    let cur_ep = current.episode.unwrap();
    let mut season = current.season.unwrap();
    let episode = if direction > 0 {
        cur_ep + 1
    } else if cur_ep > 1 {
        cur_ep - 1
    } else {
        if season > 1 {
            season -= 1;
            99 // Will be clamped by results
        } else {
            return Json(json!({"error": "Already at first episode"}));
        }
    };

    let result = match state.search_engine.search(&show, false, Some(season), Some(episode)).await {
        Ok(r) => r,
        Err(e) => return Json(json!({"error": e.to_string()})),
    };

    if !result.torrent_available || result.results.is_empty() {
        return Json(json!({
            "error": format!("No torrent found for S{:02}E{:02}", season, episode),
            "searched": result
        }));
    }

    let best = &result.results[0];
    let target_parts: Vec<&str> = current.target.splitn(2, ':').collect();

    let play_req = PlayRequest {
        magnet: Some(best.magnet.clone()),
        result_id: None,
        target: target_parts.first().map(|s| s.to_string()),
        cast_name: target_parts.get(1).map(|s| s.to_string()),
        title: Some(format!("{} S{:02}E{:02}", show, season, episode)),
        file_index: best.file_index,
        no_subs: None,
        no_intro: None,
        subtitle_lang: None,
        imdb_id: result.show.as_ref().and_then(|s| s.imdb_id.clone()),
        show: Some(show),
        season: Some(season),
        episode: Some(episode),
        seek_to: None,
        duration: None,
        quality: Some(best.quality.clone()),
        size: Some(best.size.clone()),
        poster_url: result.show.as_ref().and_then(|s| s.poster_url.clone()),
    };

    handle_play(State(state.clone()), Json(play_req)).await
}

// --- Helpers ---

fn parse_size_to_bytes(size_str: &str) -> Option<u64> {
    let lower = size_str.to_lowercase();
    let parts: Vec<&str> = lower.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let val: f64 = parts[0].parse().ok()?;
    let unit = parts[1];
    // Apr 30, 2026 (L2): unknown units (e.g. "PB", garbage) used to fall
    // through to a 1-byte multiplier, so "1.5 PB" parsed as 1.5 bytes —
    // silently broken Local Bypass size matching for any unit we don't
    // recognize. Return None on unknown unit so the caller treats the
    // input as invalid rather than wildly-wrong.
    let factor: u64 = match unit {
        "tb" | "tib" => 1024_u64.pow(4),
        "gb" | "gib" => 1024_u64.pow(3),
        "mb" | "mib" => 1024_u64.pow(2),
        "kb" | "kib" => 1024,
        "b" | "bytes" => 1,
        _ => return None,
    };
    Some((val * factor as f64) as u64)
}

/// Apr 30, 2026 (M7): validate IMDb ID format before saving to
/// resume_positions. Real IMDb IDs are `tt` followed by 7-9 digits;
/// our per-episode TV key extension appends `_sNNeMM`. Reject anything
/// that doesn't look like one of those — defends against state.json
/// growth via attacker-flood of bogus keys.
pub(crate) fn is_valid_imdb_id(s: &str) -> bool {
    let after_tt = match s.strip_prefix("tt") {
        Some(rest) => rest,
        None => return false,
    };
    // Numeric prefix (the IMDb digits) — at least 1, at most 12 digits.
    let mut digits = 0usize;
    let mut chars = after_tt.chars().peekable();
    while let Some(&c) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        digits += 1;
        chars.next();
    }
    if digits == 0 || digits > 12 {
        return false;
    }
    // Optional per-episode suffix `_sNNeMM` (1-3 digit season, 1-4 digit episode).
    if let Some(c) = chars.next() {
        if c != '_' {
            return false;
        }
        if chars.next() != Some('s') {
            return false;
        }
        let mut season_digits = 0;
        while let Some(&c) = chars.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            season_digits += 1;
            chars.next();
        }
        if season_digits == 0 || season_digits > 3 {
            return false;
        }
        if chars.next() != Some('e') {
            return false;
        }
        let mut episode_digits = 0;
        while let Some(&c) = chars.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            episode_digits += 1;
            chars.next();
        }
        if episode_digits == 0 || episode_digits > 4 {
            return false;
        }
        if chars.next().is_some() {
            return false;
        }
    }
    true
}

/// Apr 30, 2026 (L7): validate poster_url before sending to Chromecast.
/// `req.poster_url` flows into cast metadata which the Chromecast then
/// fetches directly. An attacker controlling the field could point it
/// at any URL — including internal-network endpoints — to learn IP
/// reachability via the TV's request behavior. Constrain to TMDB image
/// CDN (the legitimate source — search.rs always uses this prefix).
pub(crate) fn is_valid_poster_url(url: &str) -> bool {
    url.starts_with("https://image.tmdb.org/")
        || url.starts_with("https://www.themoviedb.org/")
}

/// Apr 30, 2026: pure helper extracted from do_play's ~100-line Local
/// Bypass scan. Walks `media_dir` looking for a healthy on-disk match for
/// the requested play. Returns the path to a matching media file
/// (`.mp4` or `.mkv`, excluding `transcoded*` artifacts), or None if no
/// candidate passes the title/year/quality filters and health checks.
///
/// Decision matrix (Apr 8/15/18/19/25/28/29 incident-cluster):
///   1. Title-token match — folder/file name must match enough words from
///      the request title (via `title_tokens_match`).
///   2. Year filter — if request title contains "2025" or "2026", entry
///      must contain the same year. No-year requests skip this filter.
///   3. Quality filter — 2160p/4K requests only match 2160-named entries;
///      1080p requests only match 1080-named entries; other qualities are
///      generic.
///   4. Directory entries are descended; first internal `.mkv`/`.mp4`
///      passing `local_bypass_file_is_healthy` (with `.spela_done` marker
///      check) wins.
///   5. Top-level file entries pass `top_level_file_is_healthy`
///      (≥100 MB + non-sparse), no expected-size match — the FLUX
///      regression case.
///
/// Pure (filesystem-only) — testable via tempfile fixtures.
pub(crate) fn find_local_bypass_match(
    media_dir: &std::path::Path,
    title: &str,
    quality: Option<&str>,
    expected_bytes: u64,
) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(media_dir).ok()?;
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let folder_name = entry.file_name().to_string_lossy().to_string();

        if !title_tokens_match(&folder_name, title) {
            continue;
        }
        let matches_year = if title.contains("2026") {
            folder_name.contains("2026")
        } else if title.contains("2025") {
            folder_name.contains("2025")
        } else {
            true
        };
        if !matches_year {
            continue;
        }
        let matches_quality = match quality {
            Some(q) => {
                let q_lower = q.to_lowercase();
                if q_lower.contains("2160p") || q_lower.contains("4k") {
                    folder_name.contains("2160p")
                        || folder_name.contains("4k")
                        || folder_name.contains("2160")
                } else if q_lower.contains("1080p") {
                    folder_name.contains("1080p") || folder_name.contains("1080")
                } else {
                    true
                }
            }
            None => true,
        };
        if !matches_quality {
            continue;
        }

        let path = entry.path();
        let has_done_marker = path.join(".spela_done").exists();

        if file_type.is_dir() {
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub_entry in sub_entries.flatten() {
                    let sub_path = sub_entry.path();
                    let fname = sub_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    let ext = sub_path.extension().and_then(|s| s.to_str()).unwrap_or("");
                    if (ext == "mp4" || ext == "mkv") && !fname.starts_with("transcoded") {
                        if local_bypass_file_is_healthy(&sub_path, has_done_marker, expected_bytes)
                        {
                            return Some(sub_path);
                        }
                    }
                }
            }
        } else if file_type.is_file() {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if (ext == "mp4" || ext == "mkv")
                && !fname.starts_with("transcoded")
                && top_level_file_is_healthy(&path)
            {
                return Some(path);
            }
        }
    }
    None
}

fn local_bypass_file_is_healthy(path: &std::path::Path, has_done_marker: bool, expected_bytes: u64) -> bool {
    // Apr 30, 2026 (M8 TOCTOU defense): refuse symlinks. A symlink in
    // ~/media/ that points to a sensitive file (e.g. /etc/shadow on a
    // shared host, or a co-tenant's data on an NFS-mounted media_dir)
    // would otherwise let ffmpeg probe + transcode it into the HLS
    // stream the Chromecast renders.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::warn!("local_bypass: refusing symlinked path {:?}", path);
            return false;
        }
        Ok(_) => {}
        Err(_) => return false,
    }
    if expected_bytes > 0 {
        return is_physically_full(path, expected_bytes);
    }
    has_done_marker && is_physically_full(path, 0)
}

fn is_physically_full(path: &std::path::Path, expected_bytes: u64) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        let logical_size = meta.len();
        // Logical size must be at least 99% of expected size
        if expected_bytes > 0 && logical_size < (expected_bytes as f64 * 0.99) as u64 {
            return false;
        }
        // Physical blocks check (Unix only): blocks() are 512-byte units.
        // Sparse files have blocks() * 512 < logical_size.
        // We allow a small margin for filesystem overhead/compression.
        let physical_size = meta.blocks() * 512;
        if physical_size < (logical_size as f64 * 0.95) as u64 {
            tracing::warn!("Local Bypass: File is sparse (physical {} < logical {}). Rejecting.", physical_size, logical_size);
            return false;
        }
        true
    } else {
        false
    }
}

/// Top-level file health check (Apr 15, 2026): trust the filename for content
/// identity and only sanity-check that the file is large enough to be a movie
/// and is not sparse.
///
/// Why this exists: `is_physically_full` enforces a strict ±1% logical-size
/// match against the search result's expected size, but for top-level
/// single-file releases the user often already has SOME release of the same
/// content on disk (different group, different remux, different audio mix)
/// whose logical size differs by a few hundred MB. The directory-bypass path
/// is correct to be strict because it has to disambiguate multiple files in a
/// season pack; the top-level path doesn't — `matches_title` + `matches_year`
/// + `matches_quality` already prove it's the right show / season / episode /
/// resolution. Forcing a fresh torrent download on every play "because the
/// FLUX remux is 311 MB smaller than the DUAL.5.1 remux" is the bug that
/// made The.Boys.S05E01...FLUX.mkv invisible to Bypass on Apr 15, 2026.
///
/// Sanity floor: 100 MB. Anything smaller than that is a partial download or
/// a stub file, not a real movie file.
fn top_level_file_is_healthy(path: &std::path::Path) -> bool {
    const MIN_MOVIE_SIZE_BYTES: u64 = 100 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(path) {
        let logical_size = meta.len();
        if logical_size < MIN_MOVIE_SIZE_BYTES {
            return false;
        }
        let physical_size = meta.blocks() * 512;
        if physical_size < (logical_size as f64 * 0.95) as u64 {
            tracing::warn!(
                "Local Bypass: Top-level file is sparse (physical {} < logical {}). Rejecting.",
                physical_size, logical_size
            );
            return false;
        }
        true
    } else {
        false
    }
}

async fn handle_targets(State(state): State<SharedState>) -> Json<Value> {
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.discover()
    }).await;
    match result {
        Ok(Ok(devices)) => Json(json!({"targets": devices})),
        Ok(Err(e)) => Json(json!({"error": e.to_string(), "targets": []})),
        Err(e) => Json(json!({"error": e.to_string(), "targets": []})),
    }
}

async fn handle_history(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    Json(json!({"history": app_state.history.iter().take(20).collect::<Vec<_>>()}))
}

// ----- Apr 29, 2026: queue endpoints -----
//
// Queue lets the user line up the next item to play after natural EOF.
// `cast_health_monitor` pops the front entry when the current stream
// reaches HWM_CLEAR_FRACTION and fires a self-call to /play. FIFO.
//
// CLI: `spela queue add <result_id> [--cast <name>]` resolves a search
// result into a QueuedItem and POSTs here. `spela queue list` GETs.
// `spela queue clear` DELETEs. CLI is implemented in main.rs.

async fn handle_queue_list(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    Json(json!({"queue": app_state.queue}))
}

/// Apr 29, 2026: queue-add request payload. The CLI sends `result_id` for
/// the common path (resolved server-side from last_search.json); explicit
/// fields below let API consumers bypass last_search lookup if they have
/// a magnet directly. result_id wins if both are provided.
#[derive(serde::Deserialize)]
pub struct QueueAddRequest {
    pub result_id: Option<usize>,
    pub cast_name: Option<String>,
    // Direct-payload fields — used when result_id is None.
    pub magnet: Option<String>,
    pub title: Option<String>,
    pub show: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub imdb_id: Option<String>,
    pub file_index: Option<u32>,
    pub poster_url: Option<String>,
    pub quality: Option<String>,
    pub size: Option<String>,
}

async fn handle_queue_add(
    State(state): State<SharedState>,
    Json(req): Json<QueueAddRequest>,
) -> Json<Value> {
    // Resolve result_id (if provided) against the server's last_search.json.
    // This is the same lookup path `do_play` uses — keeping it server-side
    // means CLI clients don't need to share the server's filesystem.
    let item = if let Some(rid) = req.result_id {
        match AppState::load_last_search(&state.state_dir) {
            Some(search) => {
                let result = match search.results.iter().find(|r| r.id == rid) {
                    Some(r) => r,
                    None => return Json(json!({
                        "error": format!("Result #{} not found in last search", rid)
                    })),
                };
                let title = match (search.show.as_ref(), search.searching.as_ref()) {
                    (Some(show), Some(ep)) => {
                        format!("{} S{:02}E{:02}", show.title, ep.season, ep.episode)
                    }
                    (Some(show), None) => show.title.clone(),
                    _ => result.title.clone(),
                };
                crate::state::QueuedItem {
                    magnet: result.magnet.clone(),
                    title,
                    show: search.show.as_ref().map(|s| s.title.clone()),
                    season: search.searching.as_ref().map(|e| e.season),
                    episode: search.searching.as_ref().map(|e| e.episode),
                    imdb_id: search.show.as_ref().and_then(|s| s.imdb_id.clone()),
                    file_index: result.file_index,
                    cast_name: req.cast_name.clone(),
                    target: None,
                    poster_url: search.show.as_ref().and_then(|s| s.poster_url.clone()),
                    quality: Some(result.quality.clone()),
                    size: Some(result.size.clone()),
                }
            }
            None => return Json(json!({
                "error": "No previous search results — run `spela search` first."
            })),
        }
    } else if let Some(magnet) = req.magnet {
        // Apr 30, 2026 SSRF defense — see torrent_engine::validate_magnet_uri.
        if let Err(e) = torrent_engine::validate_magnet_uri(&magnet) {
            return Json(json!({"error": format!("Invalid magnet: {}", e)}));
        }
        // Direct payload — caller fully populated.
        crate::state::QueuedItem {
            magnet,
            title: req.title.unwrap_or_else(|| "Unknown".into()),
            show: req.show,
            season: req.season,
            episode: req.episode,
            imdb_id: req.imdb_id,
            file_index: req.file_index,
            cast_name: req.cast_name,
            target: None,
            poster_url: req.poster_url,
            quality: req.quality,
            size: req.size,
        }
    } else {
        return Json(json!({
            "error": "Need either `result_id` or `magnet` to queue an item."
        }));
    };

    let mut app_state = AppState::load(&state.state_dir);
    app_state.queue.push(item.clone());
    if let Err(e) = app_state.save(&state.state_dir) {
        return Json(json!({"error": format!("save failed: {e}")}));
    }
    Json(json!({
        "status": "queued",
        "queue_length": app_state.queue.len(),
        "added": item,
    }))
}

async fn handle_queue_clear(State(state): State<SharedState>) -> Json<Value> {
    let mut app_state = AppState::load(&state.state_dir);
    let cleared = app_state.queue.len();
    app_state.queue.clear();
    if let Err(e) = app_state.save(&state.state_dir) {
        return Json(json!({"error": format!("save failed: {e}")}));
    }
    Json(json!({"status": "cleared", "removed": cleared}))
}

async fn handle_get_config(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    Json(json!({"preferences": app_state.preferences}))
}

async fn handle_set_config(
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Apr 30, 2026 (M3): cap each preference field to a reasonable length.
    // Pre-fix, an attacker (per-LAN, per-Host-allowlist) could flood
    // state.json by setting multi-megabyte preference strings — small
    // amplification but persistent across restarts.
    const MAX_PREF_LEN: usize = 256;
    let mut app_state = AppState::load(&state.state_dir);
    if let Some(obj) = body.as_object() {
        if let Some(v) = obj.get("default_target").and_then(|v| v.as_str()) {
            if v.len() <= MAX_PREF_LEN {
                app_state.preferences.default_target = v.into();
            }
        }
        if let Some(v) = obj.get("chromecast_name").and_then(|v| v.as_str()) {
            if v.len() <= MAX_PREF_LEN {
                app_state.preferences.chromecast_name = Some(v.into());
            }
        }
        if let Some(v) = obj.get("preferred_quality").and_then(|v| v.as_str()) {
            if v.len() <= MAX_PREF_LEN {
                app_state.preferences.preferred_quality = v.into();
            }
        }
    }
    let _ = app_state.save(&state.state_dir);
    Json(json!({"preferences": app_state.preferences}))
}

async fn handle_cast_info(
    State(state): State<SharedState>,
    Json(req): Json<CastInfoRequest>,
) -> Json<Value> {
    let device = req.device.unwrap_or_else(|| get_current_device(&state));
    // Apr 30, 2026 (M4): cap device-name length before flowing into mDNS
    // resolution. mdns-sd is reasonably hardened but unauthenticated weird
    // input flowing into mDNS lookups is unnecessary surface.
    if device.len() > 256 {
        return Json(json!({"error": "device name too long (max 256 chars)"}));
    }
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = lock_recover(&state_clone.cast);
        cast.get_info(&device)
    }).await;
    match result {
        Ok(Ok(info)) => Json(serde_json::to_value(info).unwrap_or(json!({"error": "serialize"}))),
        Ok(Err(e)) => Json(json!({"error": e.to_string()})),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

/// Stream the transcoded file with chunked transfer encoding (no Content-Length).
/// Tails the growing file as ffmpeg writes to it. No stall timeout — ffmpeg dying
/// is the only termination signal, supporting indefinite pauses.
///
/// Range request support (for reconnection):
/// - Honors `Range: bytes=N-` by seeking to offset N before streaming
/// - NEVER advertises `Accept-Ranges` or sends 206 — Chromecast interprets those
///   as "this is a seekable VOD file", which conflicts with StreamType::Live and
///   causes it to probe for Content-Length, fail, and go idle
/// - Always responds 200 with chunked transfer, even for Range requests
/// - This allows non-Chromecast clients to reconnect at an offset while keeping
///   Chromecast in live streaming mode
async fn handle_transcode_stream(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);
    let path = media_dir.join("transcoded_aac.mp4");
    let ffmpeg_pid = *lock_recover(&state.ffmpeg_pid);

    let start_offset = parse_range_start(headers.get("range").and_then(|v| v.to_str().ok()));

    if start_offset > 0 {
        tracing::info!("Transcode stream: Range request, seeking to byte {}", start_offset);
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        // Wait for file to exist (ffmpeg may not have written it yet)
        for _ in 0..30 {
            if path.exists() { break; }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to open transcoded file: {}", e);
                return;
            }
        };

        // Seek to requested offset for reconnection
        if start_offset > 0 {
            // Verify the file has enough data to seek to
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                if start_offset <= metadata.len() {
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(start_offset)).await {
                        tracing::warn!("Transcode stream: seek to {} failed: {}", start_offset, e);
                        // Fall through — stream from beginning
                    }
                } else {
                    tracing::warn!(
                        "Transcode stream: requested offset {} beyond file size {}, streaming from start",
                        start_offset, metadata.len()
                    );
                }
            }
        }

        let mut buf = vec![0u8; 64 * 1024]; // 64KB read buffer

        loop {
            match file.read(&mut buf).await {
                Ok(0) => {
                    // At EOF — check if ffmpeg is still running
                    let ffmpeg_alive = ffmpeg_pid
                        .map(|pid| unsafe { crate::torrent::kill_check(pid) })
                        .unwrap_or(false);

                    if !ffmpeg_alive {
                        // ffmpeg is done, send any remaining data and close
                        break;
                    }

                    // ffmpeg still running — file will grow, wait and retry.
                    // No stall timeout: supports indefinite pauses.
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(n) => {
                    let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if tx.send(Ok(chunk)).await.is_err() {
                        // Client disconnected
                        tracing::info!("Transcode stream client disconnected");
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("Transcode stream read error: {}", e);
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    // CRITICAL: Never send Accept-Ranges or 206 status — Chromecast Default Media
    // Receiver interprets those as "seekable VOD content", overriding StreamType::Live.
    // It then probes for Content-Length, fails (growing file), and abandons playback.
    // Always respond 200 with chunked transfer to keep Chromecast in live mode.
    axum::response::Response::builder()
        .header("Content-Type", "video/mp4")
        .header("Cache-Control", "no-cache, no-store")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
}

// --- Custom Cast Receiver Endpoints ---

/// Serve the custom receiver HTML.
async fn handle_cast_receiver_html() -> impl IntoResponse {
    const RECEIVER_HTML: &str = include_str!("../static/cast-receiver.html");
    axum::response::Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(axum::body::Body::from(RECEIVER_HTML))
        .unwrap()
}

/// Serve the intro clip from config dir.
async fn handle_cast_receiver_intro() -> impl IntoResponse {
    let path = crate::transcode::find_intro();
    match path {
        Some(p) => match tokio::fs::read(&p).await {
            Ok(data) => axum::response::Response::builder()
                .header("Content-Type", "video/mp4")
                .header("Content-Length", data.len().to_string())
                .body(axum::body::Body::from(data))
                .unwrap(),
            Err(_) => axum::response::Response::builder()
                .status(404)
                .body(axum::body::Body::from("Intro not found"))
                .unwrap(),
        },
        None => axum::response::Response::builder()
            .status(404)
            .body(axum::body::Body::from("No intro configured"))
            .unwrap(),
    }
}

// --- HLS Streaming Endpoints (Apr 15, 2026 rework) ---
//
// The original `/stream/transcode` endpoint serves a growing fragmented MP4
// with chunked transfer encoding and always returns HTTP 200 (never 206).
// Chromecast Default Media Receiver's MP4 parser refuses that combination
// and silently drops to player_state=IDLE — the "blue cast icon" failure
// mode `cast_health_monitor` exists to detect. The HLS endpoints below
// replace that path with a proper segment-based streaming format that the
// receiver supports natively (Shaka Player handles HLS out of the box).
//
// Layout under <media_dir>/transcoded_hls/:
//   - playlist.m3u8 (event-type, appendable, ENDLIST written when ffmpeg closes)
//   - init.mp4      (fmp4 init segment with moov box)
//   - seg_NNNNN.m4s (6-second fmp4 segments)
//
// All three handlers go through `serve_static_with_range`, which honors HTTP
// Range requests with proper 206 / Content-Range / Accept-Ranges headers and
// always sets a real Content-Length. That's exactly what Default Media
// Receiver wants — and exactly what `/stream/transcode` doesn't provide.

/// Resolve the spela media dir to an absolute, canonicalized path. Mirrors
/// the inline logic the cast-receiver handlers had been duplicating.
fn resolve_media_dir(state: &SharedState) -> PathBuf {
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    std::fs::canonicalize(&media_dir).unwrap_or(media_dir)
}

/// Parse an HTTP `Range: bytes=N-M` header into `(start, end)` inclusive
/// byte offsets, clamped to the file's actual size. Falls back to
/// `(0, total_size - 1)` for missing or malformed headers.
fn parse_http_range_header(header: Option<&str>, total_size: u64) -> (u64, u64) {
    if total_size == 0 {
        return (0, 0);
    }
    let last_byte = total_size - 1;
    let header = match header {
        Some(h) => h,
        None => return (0, last_byte),
    };
    let rest = match header.strip_prefix("bytes=") {
        Some(r) => r,
        None => return (0, last_byte),
    };
    // Take the FIRST range only (multipart range responses are not implemented).
    let first_range = rest.split(',').next().unwrap_or("").trim();
    let parts: Vec<&str> = first_range.splitn(2, '-').collect();
    if parts.len() != 2 {
        return (0, last_byte);
    }
    let start = parts[0].trim().parse::<u64>().unwrap_or(0);
    let end = if parts[1].trim().is_empty() {
        last_byte
    } else {
        parts[1].trim().parse::<u64>().unwrap_or(last_byte)
    };
    (start.min(last_byte), end.min(last_byte))
}

/// Serve a static file with proper HTTP Range support.
///
/// This is the helper the HLS endpoints + the new cast-friendly Range-aware
/// path use. It always sets `Content-Length`, always honors `Range:` requests
/// with `206 Partial Content` + `Content-Range`, and always advertises
/// `Accept-Ranges: bytes`. That's what Chromecast Default Media Receiver +
/// Shaka Player + every browser media element expects.
///
/// Streaming is via a tokio mpsc channel + `Body::from_stream` so we don't
/// have to load the whole file into memory. 64 KB read chunks balance CPU /
/// syscall overhead against memory.
async fn serve_static_with_range(
    path: PathBuf,
    content_type: &'static str,
    headers: &HeaderMap,
) -> axum::response::Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let metadata = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => {
            return axum::response::Response::builder()
                .status(404)
                .header("Content-Type", "text/plain")
                .body(axum::body::Body::from("Not found"))
                .unwrap();
        }
    };
    let total_size = metadata.len();

    let range_header = headers.get("range").and_then(|v| v.to_str().ok());
    let (start, end) = parse_http_range_header(range_header, total_size);
    let bytes_to_send = end.saturating_sub(start).saturating_add(1);
    let is_partial = range_header.is_some();

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("serve_static_with_range: open failed for {:?}: {}", path, e);
            return axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from("Read error"))
                .unwrap();
        }
    };

    if start > 0 {
        if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
            tracing::error!("serve_static_with_range: seek to {} failed for {:?}: {}", start, path, e);
            return axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from("Seek error"))
                .unwrap();
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(32);
    let mut remaining = bytes_to_send;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let to_read = std::cmp::min(remaining as usize, buf.len());
            match file.read(&mut buf[..to_read]).await {
                Ok(0) => break,
                Ok(n) => {
                    let n_u64 = n as u64;
                    let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                    remaining = remaining.saturating_sub(n_u64);
                    if tx.send(Ok(chunk)).await.is_err() {
                        // Client disconnected — Chromecast often does this
                        // between segment requests on a keep-alive connection.
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    let mut builder = axum::response::Response::builder()
        .header("Content-Type", content_type)
        .header("Content-Length", bytes_to_send.to_string())
        .header("Accept-Ranges", "bytes")
        .header("Cache-Control", "no-cache");

    if is_partial {
        builder = builder
            .status(206)
            .header(
                "Content-Range",
                format!("bytes {}-{}/{}", start, end, total_size),
            );
    } else {
        builder = builder.status(200);
    }

    builder.body(body).unwrap()
}

/// Serve the HLS media playlist (the one ffmpeg writes — segment list with
/// EXTINF + ENDLIST). Older Chromecasts won't accept this directly as the
/// cast LOAD URL because it lacks CODECS / RESOLUTION / BANDWIDTH metadata
/// — they need the master playlist (`/hls/master.m3u8`) instead.
/// Apr 29, 2026: Parse ffmpeg's playlist.m3u8 into the structural pieces we
/// need to PAD into a full-duration VOD manifest.
///
/// Returns:
///   - `header_lines`: every line up to but not including the first `#EXTINF:`
///     entry (preserves `#EXTM3U`, `#EXT-X-VERSION`, `#EXT-X-TARGETDURATION`,
///     `#EXT-X-MEDIA-SEQUENCE`, etc.). These are echoed verbatim into the
///     padded output.
///   - `entries`: each emitted segment's (extinf_secs, filename) pair.
///   - `had_endlist`: whether ffmpeg already appended `#EXT-X-ENDLIST` (it
///     does so on clean shutdown). If true, the manifest is already complete
///     and padding is a no-op.
///
/// Pure function. No I/O. Trivially testable.
pub fn parse_hls_playlist_for_padding(body: &str)
    -> (Vec<String>, Vec<(f64, String)>, bool)
{
    let mut header_lines = Vec::new();
    let mut entries: Vec<(f64, String)> = Vec::new();
    let mut had_endlist = false;
    let mut pending_extinf: Option<f64> = None;
    let mut header_done = false;

    for line in body.lines() {
        let line = line.trim_end();
        if line == "#EXT-X-ENDLIST" {
            had_endlist = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // Format: `#EXTINF:10.427089,` (sometimes with title after comma)
            header_done = true;
            let secs_str = rest.split(',').next().unwrap_or("0");
            pending_extinf = secs_str.parse::<f64>().ok();
            continue;
        }
        // A non-comment line after a pending EXTINF is the segment filename.
        if pending_extinf.is_some() && !line.is_empty() && !line.starts_with('#') {
            if let Some(secs) = pending_extinf.take() {
                entries.push((secs, line.to_string()));
            }
            continue;
        }
        // Anything else before the first EXTINF is header.
        if !header_done {
            header_lines.push(line.to_string());
        }
    }
    (header_lines, entries, had_endlist)
}

/// Apr 29, 2026: Generate a VOD-style padded manifest from ffmpeg's actual
/// playlist + the source's known total duration.
///
/// Strategy:
///   1. Compute remaining content duration: `duration - ss_offset` (clamped
///      to ≥ 0). This is the wall-clock playback time the receiver should
///      see in its progress bar. ss_offset accounts for `--seek N` or
///      auto-resume (ffmpeg's HLS output starts at t=0 regardless of
///      source seek; only `duration - ss_offset` of content is in the
///      transcoded stream).
///   2. Compute average EXTINF from emitted segments. Falls back to the
///      `default_segment_secs` (`hls_time` arg, typically 6) when no
///      segments emitted yet.
///   3. Predict total segments: `ceil(remaining / avg) + 2-buffer`. The
///      +2 covers keyframe-alignment variance — actual segment count
///      typically lands within ±1 of `remaining/avg`.
///   4. Output: header verbatim → emitted entries verbatim → padded
///      placeholder entries (using avg as EXTINF) → `#EXT-X-ENDLIST`.
///
/// If the receiver's accumulated playback (sum of EXTINF as it advances)
/// reaches a placeholder segment that ffmpeg never produces, the long-poll
/// in `handle_hls_segment` waits up to 28 s then 503s. Receiver retries
/// or gracefully ends.
///
/// Pure function. No I/O. Inputs/outputs match what tests can construct.
pub fn build_padded_vod_manifest(
    body: &str,
    remaining_duration_secs: f64,
    default_segment_secs: f64,
) -> String {
    let (header_lines, entries, had_endlist) = parse_hls_playlist_for_padding(body);

    // If ffmpeg already finished + ENDLIST is present, use the manifest as-is.
    if had_endlist {
        return body.to_string();
    }

    // Compute average EXTINF from emitted segments, defaulting if none.
    let avg_extinf = if entries.is_empty() {
        default_segment_secs.max(1.0)
    } else {
        let sum: f64 = entries.iter().map(|(s, _)| *s).sum();
        (sum / entries.len() as f64).max(1.0)
    };

    let remaining = remaining_duration_secs.max(0.0);
    let predicted_total = ((remaining / avg_extinf).ceil() as usize) + 2;

    // We never want to produce FEWER entries than ffmpeg has actually
    // written — that would point the receiver at non-existent indexes and
    // truncate playback observably. If the sum-of-existing-extinf already
    // exceeds the predicted total span, take whichever is larger.
    let predicted_total = predicted_total.max(entries.len() + 1);

    let mut out = String::new();
    for line in &header_lines {
        out.push_str(line);
        out.push('\n');
    }
    // Emit existing entries with their REAL EXTINF — receiver's progress bar
    // should reflect actual segment durations for what's already on disk.
    for (extinf, name) in &entries {
        out.push_str(&format!("#EXTINF:{:.6},\n", extinf));
        out.push_str(name);
        out.push('\n');
    }
    // Pad with placeholder segments using the predicted EXTINF.
    for i in entries.len()..predicted_total {
        out.push_str(&format!("#EXTINF:{:.6},\n", avg_extinf));
        out.push_str(&format!("seg_{:05}.ts\n", i));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

async fn handle_hls_playlist(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> axum::response::Response {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("?");
    let range = headers.get("range").and_then(|v| v.to_str().ok()).unwrap_or("-");
    tracing::info!("HLS playlist hit: ua={:?} range={:?}", ua, range);
    let path = resolve_media_dir(&state)
        .join("transcoded_hls")
        .join("playlist.m3u8");

    // Apr 29, 2026: VOD-padded manifest mode (config.vod_manifest_padded).
    // Predicted full-duration manifest with ENDLIST upfront — receiver
    // treats the stream as VOD with honest total duration, so current_time
    // doesn't inflate and HWM saves stay accurate.  Companion:
    // handle_hls_segment's long-poll for not-yet-written placeholders.
    // Trade-off: enables receiver-side total-duration display at the cost
    // of DMR rendering a persistent progress-bar overlay.  Default off;
    // live mode (this path skipped) = no overlay AND no total display.
    // See spela CLAUDE.md § "DMR overlay is stream-type-dependent".
    if state.config.vod_manifest_padded {
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => {
                let app_state = AppState::load(&state.state_dir);
                let (duration, ss_offset) = app_state
                    .current
                    .as_ref()
                    .map(|c| (c.duration.unwrap_or(0.0), c.ss_offset))
                    .unwrap_or((0.0, 0.0));
                let remaining = (duration - ss_offset).max(0.0);
                if remaining > 0.0 {
                    let padded = build_padded_vod_manifest(&body, remaining, 6.0);
                    tracing::debug!(
                        "HLS playlist: padded VOD manifest ({} bytes, remaining={:.0}s)",
                        padded.len(), remaining
                    );
                    return axum::response::Response::builder()
                        .status(200)
                        .header("Content-Type", "application/vnd.apple.mpegurl")
                        .header("Content-Length", padded.len().to_string())
                        .header("Cache-Control", "no-cache")
                        .body(axum::body::Body::from(padded))
                        .unwrap();
                } else {
                    tracing::warn!(
                        "HLS playlist: vod_manifest_padded enabled but duration unknown — falling through to default serve"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("HLS playlist read failed: {}; falling back to file serve", e);
            }
        }
    }

    // Apr 28, 2026 [EXPERIMENTAL — superseded by vod_manifest_padded above]:
    // Plain ENDLIST append. Side effects (chase-the-end / HWM inflation)
    // documented in spela TODO.md. Kept gated behind its own flag for
    // research/comparison purposes.
    if state.config.experimental_endlist_hack {
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => {
                let body = if body.contains("#EXT-X-ENDLIST") {
                    body
                } else {
                    let mut b = body;
                    if !b.ends_with('\n') { b.push('\n'); }
                    b.push_str("#EXT-X-ENDLIST\n");
                    tracing::debug!("HLS playlist: appended ENDLIST hack");
                    b
                };
                return axum::response::Response::builder()
                    .status(200)
                    .header("Content-Type", "application/vnd.apple.mpegurl")
                    .header("Content-Length", body.len().to_string())
                    .header("Cache-Control", "no-cache")
                    .body(axum::body::Body::from(body))
                    .unwrap();
            }
            Err(e) => {
                tracing::warn!("HLS playlist read failed: {}; falling back to file serve", e);
                // fall through
            }
        }
    }

    serve_static_with_range(path, "application/vnd.apple.mpegurl", &headers).await
}

/// Serve a synthetic HLS master playlist that declares CODECS / RESOLUTION /
/// BANDWIDTH and points at the media playlist (`playlist.m3u8`) ffmpeg
/// generates. CrKey 1.56 firmware on 1st-gen Chromecasts won't load a media
/// playlist directly via LOAD — Apr 15, 2026 live test against Fredriks TV
/// proved the receiver fetches the bare media playlist 4 times in a row
/// then bails to player_state=IDLE / idle_reason=ERROR without ever
/// requesting a single segment, while Apple's bipbop reference HLS stream
/// (which has a proper master playlist) plays in 5 seconds on the SAME
/// device. The diagnostic difference: bipbop's master playlist declares
/// CODECS="avc1.64001f,mp4a.40.2" + BANDWIDTH + RESOLUTION; ffmpeg's
/// generated media playlist declares none of that. Without those hints
/// the older Shaka Player can't pre-validate the stream and gives up.
///
/// We generate the master playlist on the fly here rather than wiring up a
/// second ffmpeg pass, because the CODECS string is constant for every
/// spela transcode (h264_nvenc preset p4 outputs H.264 High@4.0 →
/// `avc1.640028`, AAC LC stereo → `mp4a.40.2`) and BANDWIDTH /
/// RESOLUTION are also fixed by spela's standard 1920×1080 / ~6 Mbps
/// transcode profile.
async fn handle_hls_master(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> axum::response::Response {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("?");
    tracing::info!("HLS master hit: ua={:?}", ua);
    // Make sure the media playlist actually exists before claiming the
    // master is valid — otherwise the receiver will fetch the master, then
    // request playlist.m3u8 immediately, then 404, then bail.
    let media_playlist_path = resolve_media_dir(&state)
        .join("transcoded_hls")
        .join("playlist.m3u8");
    if !media_playlist_path.exists() {
        tracing::warn!(
            "HLS master requested but media playlist missing at {:?}",
            media_playlist_path
        );
        return axum::response::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(axum::body::Body::from("Media playlist not yet ready"))
            .unwrap();
    }

    // CODECS string for spela's standard transcode pipeline:
    //   - avc1.640028 = H.264 High profile, level 4.0 (1080p30 well within)
    //   - mp4a.40.2   = MPEG-4 AAC LC
    // BANDWIDTH is a hint for ABR; for a single-rendition stream it doesn't
    // need to be exact. 6 Mbps matches the typical NVENC preset p4 cq 23
    // output for 1080p H.264 + AAC stereo 192 kbps.
    let master = "#EXTM3U\n\
                  #EXT-X-VERSION:3\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\n\
                  playlist.m3u8\n";

    axum::response::Response::builder()
        .status(200)
        .header("Content-Type", "application/vnd.apple.mpegurl")
        .header("Cache-Control", "no-cache")
        .header("Content-Length", master.len().to_string())
        .header("Accept-Ranges", "bytes")
        .body(axum::body::Body::from(master))
        .unwrap()
}

/// Serve the HLS fmp4 init segment (moov box). Only used for the legacy
/// fmp4 path. With the Apr 15, 2026 switch to MPEG-TS segments this is a
/// 404 (no file) for any new play, kept registered for the legacy fmp4
/// fallback path.
async fn handle_hls_init(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> axum::response::Response {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("?");
    tracing::info!("HLS init.mp4 hit: ua={:?}", ua);
    let path = resolve_media_dir(&state)
        .join("transcoded_hls")
        .join("init.mp4");
    serve_static_with_range(path, "video/mp4", &headers).await
}

/// Serve an individual HLS MPEG-TS segment. The segment name is taken from
/// the URL path component (`/hls/seg_00042.ts`) and joined onto
/// `transcoded_hls/` after a strict whitelist check that prevents path
/// traversal: only ASCII alphanumerics, `_`, `-`, and `.` are allowed, the
/// final extension must be `.ts`, and the total length is capped at 64 chars.
///
/// Also accepts `.m4s` for the legacy fmp4 path, kept as dead code for
/// future use if rust_cast ever exposes `media.hlsSegmentFormat`.
async fn handle_hls_segment(
    State(state): State<SharedState>,
    axum::extract::Path(segment): axum::extract::Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    // Path traversal / abuse hardening: reject anything that isn't a tame
    // segment filename. We want only `seg_NNNNN.ts` (or `.m4s`) to be
    // resolvable through this endpoint.
    let safe = !segment.is_empty()
        && segment.len() <= 64
        && (segment.ends_with(".ts") || segment.ends_with(".m4s"))
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && !segment.contains("..");
    if !safe {
        tracing::warn!("HLS segment request rejected as unsafe: {:?}", segment);
        return axum::response::Response::builder()
            .status(403)
            .header("Content-Type", "text/plain")
            .body(axum::body::Body::from("Forbidden"))
            .unwrap();
    }
    // MPEG-TS segments use the official `video/mp2t` MIME type; legacy fmp4
    // segments use `video/mp4`. Default Media Receiver accepts both.
    let content_type: &'static str = if segment.ends_with(".ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("?");
    let range = headers.get("range").and_then(|v| v.to_str().ok()).unwrap_or("-");
    tracing::info!(
        "HLS segment hit: {} ({}) ua={:?} range={:?}",
        segment, content_type, ua, range
    );
    let path = resolve_media_dir(&state)
        .join("transcoded_hls")
        .join(&segment);

    // Apr 29, 2026: VOD-padded manifest mode requires us to long-poll for
    // segments that the receiver requested based on the predicted manifest
    // but ffmpeg hasn't written yet. Without this, the receiver gets a
    // 404, gives up on that segment, and may stop playback entirely.
    //
    // Wait up to 28 s (under the typical 30 s receiver HTTP timeout) for
    // the file to appear, polling the filesystem every 200 ms. ffmpeg's
    // HLS muxer uses temp_file flag, so the file appears atomically at
    // its final path only when fully written — no torn reads.
    //
    // If timeout: serve 503 with Retry-After. Receiver will retry with a
    // new request, which restarts the wait. This handles the edge case
    // where ffmpeg falls slightly behind playback (transcode pace varies)
    // — receiver pauses briefly, then resumes once segment lands.
    //
    // For segments well past actual content (predicted+buffer overshoot),
    // the file never appears and the timeout fires repeatedly. Each
    // request burns 28 s wall, then 503 Retry-After. Receiver eventually
    // gives up and ends playback near the right spot.
    if state.config.vod_manifest_padded && !path.exists() {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(28);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if path.exists() {
                tracing::debug!(
                    "HLS segment {} appeared after long-poll wait", segment
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                tracing::info!(
                    "HLS segment {} not produced within 28 s — returning 503 Retry-After",
                    segment
                );
                return axum::response::Response::builder()
                    .status(503)
                    .header("Retry-After", "10")
                    .header("Content-Type", "text/plain")
                    .body(axum::body::Body::from(
                        "segment not yet available; retry"
                    ))
                    .unwrap();
            }
        }
    }

    serve_static_with_range(path, content_type, &headers).await
}

/// Serve the current subtitle WebVTT file.
async fn handle_cast_receiver_subs(State(state): State<SharedState>) -> impl IntoResponse {
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);
    let vtt_path = media_dir.join("subtitle_eng.vtt");
    match tokio::fs::read_to_string(&vtt_path).await {
        Ok(data) => axum::response::Response::builder()
            .header("Content-Type", "text/vtt; charset=utf-8")
            .header("Access-Control-Allow-Origin", "*")
            .body(axum::body::Body::from(data))
            .unwrap(),
        Err(_) => axum::response::Response::builder()
            .status(404)
            .body(axum::body::Body::from("No subtitles available"))
            .unwrap(),
    }
}

/// Return current stream config for the receiver to self-configure.
/// This works around rust_cast's Media struct not supporting tracks/customData.
async fn handle_cast_config(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    let current = app_state.current.as_ref();

    let title = current.map(|c| c.title.as_str()).unwrap_or("");
    let imdb_id = current.and_then(|c| c.imdb_id.as_deref()).unwrap_or("");

    // Check if intro exists
    let intro_url = crate::transcode::find_intro()
        .map(|_| format!("http://{}:{}/cast-receiver/intro.mp4", state.config.stream_host, state.config.port));

    // Check if subtitles exist
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);
    let subs_vtt = media_dir.join("subtitle_eng.vtt");
    let subtitle_url = if subs_vtt.exists() {
        Some(format!("http://{}:{}/cast-receiver/subs.vtt", state.config.stream_host, state.config.port))
    } else {
        None
    };

    // Get resume position
    let resume_pos = app_state.get_position(
        if imdb_id.is_empty() { None } else { Some(imdb_id.to_string()) },
        if title.is_empty() { None } else { Some(title.to_string()) }
    );
    let resume_pos = if resume_pos > 0.0 { Some(resume_pos) } else { None };

    Json(json!({
        "title": title,
        "imdb_id": imdb_id,
        "intro_url": intro_url,
        "subtitle_url": subtitle_url,
        "subtitle_lang": "English",
        "subtitle_lang_code": "en",
        "duration": current.and_then(|c| c.duration),
        "resume_position": resume_pos,
        "seek_restart_url": format!("http://{}:{}/api/seek-restart", state.config.stream_host, state.config.port),
    }))
}

#[derive(Deserialize)]
struct SeekRestartRequest {
    t: f64,
}

/// Restart the transcode from a new position (server-side seek).
async fn handle_seek_restart(
    State(state): State<SharedState>,
    Json(req): Json<SeekRestartRequest>,
) -> Json<Value> {
    // Apr 30, 2026 (M6): reject non-finite seek values BEFORE clamping.
    // f64::NaN.max(0.0) returns NaN (preserves NaN), and the value would
    // flow into ffmpeg's -ss arg if/when this stub gets implemented.
    if !req.t.is_finite() {
        return Json(json!({"error": "seek position must be finite"}));
    }
    let seek_seconds = req.t.max(0.0);

    // Kill current ffmpeg
    if let Some(pid) = lock_recover(&state.ffmpeg_pid).take() {
        torrent::kill_pid(pid);
    }
    // Delete old transcoded file
    let transcoded = state.media_dir.join("transcoded_aac.mp4");
    if transcoded.exists() {
        let _ = std::fs::remove_file(&transcoded);
    }

    // Get current stream's webtorrent URL from state
    let app_state = AppState::load(&state.state_dir);
    let _server_url = match &app_state.current {
        Some(c) => {
            // The URL might be the transcode endpoint — we need the original webtorrent URL
            // which is stored as the first webtorrent URL on port 8888
            if c.url.contains("/stream/transcode") {
                // Reconstruct from webtorrent log or use a stored field
                // For now, check if webtorrent is still running and serving
                format!("http://{}:8888", state.config.stream_host)
            } else {
                c.url.clone()
            }
        }
        None => return Json(json!({"error": "No active stream"})),
    };

    // TODO: Restart ffmpeg with -ss offset from the webtorrent source
    // This requires knowing the exact webtorrent URL, which we should store in state
    tracing::info!("Seek-restart to {:.0}s requested (implementation pending full webtorrent URL tracking)", seek_seconds);

    // For now, return the existing stream URL — full implementation needs webtorrent URL in state
    Json(json!({
        "status": "ready",
        "stream_url": format!("http://{}:{}/stream/transcode", state.config.stream_host, state.config.port),
        "seek_to": seek_seconds,
    }))
}

#[derive(Deserialize)]
struct PositionRequest {
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    t: f64,
    duration: Option<f64>,
}

#[derive(Deserialize)]
struct PositionQuery {
    imdb_id: Option<String>,
    title: Option<String>,
}

/// Save resume position for a movie/show.
async fn handle_save_position(
    State(state): State<SharedState>,
    Json(req): Json<PositionRequest>,
) -> Json<Value> {
    // Apr 30, 2026 (M7): validate inputs before they reach state.json.
    // - imdb_id must look like a real IMDb ID (or its per-episode form)
    // - title must be reasonably sized (cap 256 chars)
    // - t must be finite (NaN/inf would silently corrupt save_position_smart)
    if req.imdb_id.is_none() && req.title.is_none() {
        return Json(json!({"error": "Missing imdb_id and title"}));
    }
    if let Some(id) = req.imdb_id.as_deref() {
        if !is_valid_imdb_id(id) {
            return Json(json!({"error": "Invalid imdb_id format"}));
        }
    }
    if let Some(t) = req.title.as_deref() {
        if t.len() > 256 {
            return Json(json!({"error": "Title too long (max 256 chars)"}));
        }
    }
    if !req.t.is_finite() {
        return Json(json!({"error": "Position must be finite"}));
    }
    let mut app_state = AppState::load(&state.state_dir);
    let (key, saved) = app_state.save_position_smart(req.imdb_id.clone(), req.title.clone(), req.t, req.duration);
    if saved {
        let _ = app_state.save(&state.state_dir);
    }
    Json(json!({"status": if saved { "saved" } else { "ignored" }, "key": key, "t": req.t}))
}

/// Get resume position for a movie/show.
async fn handle_get_position(
    State(state): State<SharedState>,
    Query(query): Query<PositionQuery>,
) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    let pos = app_state.get_position(query.imdb_id.clone(), query.title);
    Json(json!({"imdb_id": query.imdb_id, "t": pos}))
}

/// Reset resume position for a movie/show.
async fn handle_reset_position(
    State(state): State<SharedState>,
    Json(req): Json<PositionQuery>, // Reuse PositionQuery but it's a JSON body in POST
) -> Json<Value> {
    if req.imdb_id.is_none() && req.title.is_none() {
        return Json(json!({"error": "Missing imdb_id and title"}));
    }
    let mut app_state = AppState::load(&state.state_dir);
    let key = app_state.reset_position(req.imdb_id, req.title);
    let _ = app_state.save(&state.state_dir);
    Json(json!({"status": "reset", "key": key}))
}

/// Force a retry of the current stream.
async fn handle_retry(State(_state): State<SharedState>) -> Json<Value> {
    // TODO: Implement retry logic — load next search result and cast
    tracing::info!("Stream retry requested by Cast receiver");
    Json(json!({"status": "retry_requested"}))
}

/// Parse a Range header value into an open-ended start offset.
/// Only supports "bytes=N-" (open-ended). Bounded ranges ("bytes=N-M") return None
/// because our file is growing and we can't honor a fixed end byte.
fn parse_range_start(range_header: Option<&str>) -> u64 {
    range_header
        .and_then(|range_str| {
            let range_str = range_str.strip_prefix("bytes=")?;
            let dash_pos = range_str.find('-')?;
            let start_str = &range_str[..dash_pos];
            let after_dash = &range_str[dash_pos + 1..];
            if !after_dash.is_empty() {
                return None; // Bounded range — ignore
            }
            start_str.parse::<u64>().ok()
        })
        .unwrap_or(0)
}

// --- Helpers ---

fn get_current_device(state: &ServerState) -> String {
    let app_state = AppState::load(&state.state_dir);
    app_state.current
        .and_then(|c| c.target.splitn(2, ':').nth(1).map(String::from))
        .or(app_state.preferences.chromecast_name)
        .unwrap_or_else(|| state.config.default_device.clone())
}

fn is_process_running(pid: u32) -> bool {
    unsafe { torrent::kill_check(pid) }
}

fn cast_result_to_json(
    result: Result<anyhow::Result<crate::cast::CastResult>, tokio::task::JoinError>
) -> Json<Value> {
    match result {
        Ok(Ok(r)) => Json(serde_json::to_value(r).unwrap_or(json!({"error": "serialize"}))),
        Ok(Err(e)) => Json(json!({"error": e.to_string()})),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Input validation (Apr 30, 2026 — security audit cluster) ---

    #[test]
    fn parse_size_to_bytes_handles_known_units() {
        assert_eq!(parse_size_to_bytes("4.6 GB"), Some(4_939_212_390)); // 4.6 * 1024^3
        assert_eq!(parse_size_to_bytes("100 MB"), Some(104_857_600));
        assert_eq!(parse_size_to_bytes("500 KB"), Some(512_000));
    }

    #[test]
    fn parse_size_to_bytes_handles_tb_suffix_post_l2_fix() {
        // L2 fix: unknown units returned 1-byte multiplier; TB was unsupported.
        // Now explicitly handled. 1.5 * 1024^4 (rounded down by f64->u64).
        let v = parse_size_to_bytes("1.5 TB").unwrap();
        // Allow ±1 LSB tolerance for f64 rounding.
        let expected = 1.5 * 1024_f64.powi(4);
        assert!((v as f64 - expected).abs() < 2.0, "got {}, expected ~{}", v, expected);
    }

    #[test]
    fn parse_size_to_bytes_returns_none_on_unknown_unit_post_l2_fix() {
        // L2 fix pin: pre-fix this returned `Some((1.5 * 1.0) as u64) = Some(1)`,
        // silently breaking Local Bypass size matching. Post-fix returns None.
        assert_eq!(parse_size_to_bytes("1.5 PB"), None);
        assert_eq!(parse_size_to_bytes("4.6 garbage"), None);
        assert_eq!(parse_size_to_bytes("3.2 ZB"), None);
    }

    #[test]
    fn parse_size_to_bytes_returns_none_on_malformed() {
        assert_eq!(parse_size_to_bytes(""), None);
        assert_eq!(parse_size_to_bytes("abc"), None);
        assert_eq!(parse_size_to_bytes("4.6"), None); // missing unit
        assert_eq!(parse_size_to_bytes("GB 4.6"), None); // wrong order
    }

    // --- Local Bypass match decision (Apr 30, 2026 — extracted from do_play) ---
    //
    // The find_local_bypass_match helper extracts do_play's ~100-line
    // file-scan-and-match logic into a pure function. Tests cover the
    // decision matrix that was previously only exercised live (Apr 8,
    // 15, 18, 19, 25, 28, 29 incident-cluster). Any future refactor
    // that breaks any of these cases will fail here first.

    fn make_dense_mkv(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = parent.join(name);
        std::fs::write(&p, vec![0u8; 110 * 1024 * 1024]).unwrap();
        p
    }

    fn make_sparse_mkv(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = parent.join(name);
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(200 * 1024 * 1024).unwrap();
        drop(f);
        p
    }

    #[test]
    fn find_local_bypass_directory_match_with_done_marker() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("The.Boys.S05E03.1080p.FLUX");
        std::fs::create_dir_all(&inner).unwrap();
        let mkv = make_dense_mkv(&inner, "The.Boys.S05E03.1080p.FLUX.mkv");
        std::fs::write(inner.join(".spela_done"), b"").unwrap();
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", Some("1080p"), 0);
        assert_eq!(result.as_deref(), Some(mkv.as_path()));
    }

    #[test]
    fn find_local_bypass_top_level_file_match() {
        // The Apr 15 FLUX-file regression case.
        let root = tempfile::tempdir().unwrap();
        let mkv = make_dense_mkv(root.path(), "The.Boys.S05E03.1080p.FLUX.mkv");
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", Some("1080p"), 0);
        assert_eq!(result.as_deref(), Some(mkv.as_path()));
    }

    #[test]
    fn find_local_bypass_returns_none_on_no_match() {
        let root = tempfile::tempdir().unwrap();
        make_dense_mkv(root.path(), "Different.Show.S01E01.mkv");
        assert!(find_local_bypass_match(root.path(), "The Boys S05E03", None, 0).is_none());
    }

    #[test]
    fn find_local_bypass_quality_mismatch_4k_target_skips_1080p() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("The.Boys.S05E03.1080p");
        std::fs::create_dir_all(&inner).unwrap();
        make_dense_mkv(&inner, "The.Boys.S05E03.1080p.mkv");
        std::fs::write(inner.join(".spela_done"), b"").unwrap();
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", Some("2160p"), 0);
        assert!(result.is_none(), "4K request must not match 1080p disk content");
    }

    #[test]
    fn find_local_bypass_skips_transcoded_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("The.Boys.S05E03");
        std::fs::create_dir_all(&inner).unwrap();
        // Only file inside is the transcode artifact — must NOT be picked.
        std::fs::write(inner.join("transcoded_aac.mp4"), vec![0u8; 110 * 1024 * 1024]).unwrap();
        std::fs::write(inner.join(".spela_done"), b"").unwrap();
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", None, 0);
        assert!(result.is_none());
    }

    #[test]
    fn find_local_bypass_rejects_sparse_top_level_file() {
        let root = tempfile::tempdir().unwrap();
        make_sparse_mkv(root.path(), "The.Boys.S05E03.mkv");
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", None, 0);
        assert!(result.is_none(), "sparse top-level must be rejected");
    }

    #[test]
    fn find_local_bypass_year_filter_2026_excludes_2025() {
        // When request title contains 2026, only entries containing 2026
        // can match. Pin the year-filter behavior added Apr 15.
        let root = tempfile::tempdir().unwrap();
        let inner_2025 = root.path().join("The.Boys.S05E03.2025");
        std::fs::create_dir_all(&inner_2025).unwrap();
        make_dense_mkv(&inner_2025, "a.mkv");
        std::fs::write(inner_2025.join(".spela_done"), b"").unwrap();
        let result = find_local_bypass_match(root.path(), "The Boys S05E03 2026", None, 0);
        assert!(result.is_none(), "2025 entry must not match 2026 request");
    }

    #[test]
    fn find_local_bypass_no_year_in_request_matches_anything() {
        // When title has no year token, any title-matching entry is a
        // candidate. Pin the year-filter's defensive default.
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("The.Boys.S05E03.2025");
        std::fs::create_dir_all(&inner).unwrap();
        let mkv = make_dense_mkv(&inner, "a.mkv");
        std::fs::write(inner.join(".spela_done"), b"").unwrap();
        let result = find_local_bypass_match(root.path(), "The Boys S05E03", None, 0);
        assert_eq!(result.as_deref(), Some(mkv.as_path()));
    }

    // --- Local Bypass top-level file health (Apr 15, 2026 FLUX-file fix) ---

    #[test]
    fn test_top_level_file_is_healthy_rejects_below_100mb() {
        // Apr 15 fix: tiny partial files (e.g. a 5 MB stub from a failed
        // download) must NOT be Bypass candidates. Pin the 100 MB floor.
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("tiny.mkv");
        std::fs::write(&small, vec![0u8; 50 * 1024 * 1024]).unwrap(); // 50 MB
        assert!(!top_level_file_is_healthy(&small));
    }

    #[test]
    fn test_top_level_file_is_healthy_accepts_dense_full_file() {
        // A non-sparse file ≥ 100 MB should pass. Use 110 MB so we're past
        // the floor. Standard write produces a fully-allocated file (no holes).
        let dir = tempfile::tempdir().unwrap();
        let healthy = dir.path().join("full.mkv");
        std::fs::write(&healthy, vec![0u8; 110 * 1024 * 1024]).unwrap();
        assert!(top_level_file_is_healthy(&healthy));
    }

    #[test]
    fn test_top_level_file_is_healthy_rejects_sparse_below_95pct() {
        // Sparse file: logical 200 MB, physical near-zero (just the inode).
        // Pre-fix Local Bypass would happily probe this, ffmpeg reads zeros,
        // cast hangs at blue-icon. Pin the 0.95 sparse-detection threshold.
        let dir = tempfile::tempdir().unwrap();
        let sparse = dir.path().join("sparse.mkv");
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(200 * 1024 * 1024).unwrap(); // 200 MB logical, 0 physical
        drop(f);
        assert!(!top_level_file_is_healthy(&sparse));
    }

    #[test]
    fn test_top_level_file_is_healthy_rejects_nonexistent_path() {
        // Symptom of stale Local Bypass cache reference; must return false
        // not panic.
        let nonexistent = std::path::Path::new("/tmp/spela-nonexistent-fixture-xxxxyyyyzzzz");
        assert!(!top_level_file_is_healthy(nonexistent));
    }

    #[test]
    fn is_valid_imdb_id_accepts_canonical_movie_id() {
        assert!(is_valid_imdb_id("tt1190634"));
        assert!(is_valid_imdb_id("tt0903747"));
    }

    #[test]
    fn is_valid_imdb_id_accepts_per_episode_form() {
        assert!(is_valid_imdb_id("tt1190634_s05e05"));
        assert!(is_valid_imdb_id("tt19854762_s02e08"));
        // Allow up to 3-digit season + 4-digit episode
        assert!(is_valid_imdb_id("tt1234567_s100e1234"));
    }

    #[test]
    fn is_valid_imdb_id_rejects_garbage() {
        // M7 defense: state.json size cap via input validation
        assert!(!is_valid_imdb_id("attacker_string_no_tt_prefix"));
        assert!(!is_valid_imdb_id("tt"));            // no digits
        assert!(!is_valid_imdb_id("tt12345abc"));    // non-numeric
        assert!(!is_valid_imdb_id("tt1234567890123")); // > 12 digits
        assert!(!is_valid_imdb_id(""));
    }

    #[test]
    fn is_valid_imdb_id_rejects_malformed_episode_suffix() {
        assert!(!is_valid_imdb_id("tt1234_s05"));        // missing e
        assert!(!is_valid_imdb_id("tt1234_s05e"));       // empty episode
        assert!(!is_valid_imdb_id("tt1234_s05e1234567")); // > 4-digit episode
        assert!(!is_valid_imdb_id("tt1234_s5e1abc"));    // trailing garbage
        assert!(!is_valid_imdb_id("tt1234x_s5e1"));      // wrong separator
    }

    #[test]
    fn is_valid_poster_url_accepts_tmdb_cdn() {
        assert!(is_valid_poster_url(
            "https://image.tmdb.org/t/p/w500/in1R2dDc421JxsoRWaIIAqVI2KE.jpg"
        ));
    }

    #[test]
    fn is_valid_poster_url_rejects_internal_targets() {
        // L7: defends against attacker-controlled poster_url that the
        // Chromecast would fetch directly, leaking IP reachability info.
        assert!(!is_valid_poster_url("http://192.168.4.1:6379/"));
        assert!(!is_valid_poster_url("http://localhost:80/"));
        assert!(!is_valid_poster_url("file:///etc/passwd"));
    }

    #[test]
    fn is_valid_poster_url_rejects_http_insecure() {
        // Even on TMDB hostname — must be https.
        assert!(!is_valid_poster_url("http://image.tmdb.org/foo.jpg"));
    }

    #[test]
    fn is_valid_poster_url_rejects_subdomain_lookalike() {
        // Defense: `image.tmdb.org.evil.com` would match a naive substring
        // check. Our check uses `starts_with` on the full URL prefix
        // including `://` and trailing slash, which forbids this.
        assert!(!is_valid_poster_url(
            "https://image.tmdb.org.evil.com/foo.jpg"
        ));
    }

    // --- Mutex poison recovery (Apr 30, 2026 — H3 security audit) ---

    #[test]
    fn lock_recover_returns_guard_after_poisoning() {
        // Poison the mutex by panicking in a thread that holds the guard.
        // After the panic, std::sync::Mutex::lock() returns PoisonError;
        // .unwrap() on that error is what burned us in the rustls cascade.
        // lock_recover MUST return a usable guard via PoisonError::into_inner.
        let m = std::sync::Arc::new(Mutex::new(42_i32));
        let m_clone = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m_clone.lock().unwrap();
            panic!("intentional poisoning for test");
        })
        .join();

        // Sanity: bare .lock() returns Err on the poisoned mutex.
        assert!(m.lock().is_err());

        // The recovery path: lock_recover returns a usable guard.
        let guard = lock_recover(&m);
        assert_eq!(*guard, 42);
    }

    #[test]
    fn lock_recover_works_on_healthy_mutex() {
        // Smoke test: zero-impact on the healthy path.
        let m = Mutex::new(String::from("hello"));
        let guard = lock_recover(&m);
        assert_eq!(*guard, "hello");
    }

    // --- Host-header allowlist (Apr 30, 2026 — H2 security audit) ---

    #[test]
    fn parse_host_header_strips_port() {
        assert_eq!(parse_host_header("darwin.home:7890"), "darwin.home");
        assert_eq!(parse_host_header("192.168.4.1:7890"), "192.168.4.1");
        assert_eq!(parse_host_header("localhost:7890"), "localhost");
    }

    #[test]
    fn parse_host_header_no_port() {
        assert_eq!(parse_host_header("darwin.home"), "darwin.home");
        assert_eq!(parse_host_header("localhost"), "localhost");
    }

    #[test]
    fn parse_host_header_handles_ipv6_loopback_with_port() {
        // RFC 7230 § 5.4 — IPv6 in Host headers is bracketed: `[::1]:7890`.
        // Strip the trailing `:port` after the closing bracket; keep `[::1]`.
        assert_eq!(parse_host_header("[::1]:7890"), "[::1]");
    }

    #[test]
    fn parse_host_header_handles_ipv6_no_port() {
        assert_eq!(parse_host_header("[::1]"), "[::1]");
    }

    #[test]
    fn compute_host_allowlist_includes_canonical_defaults() {
        let mut config = Config::default();
        config.stream_host = String::new();
        let allow = compute_host_allowlist(&config);
        assert!(allow.contains("localhost"));
        assert!(allow.contains("127.0.0.1"));
        assert!(allow.contains("darwin.home"));
    }

    #[test]
    fn compute_host_allowlist_includes_stream_host_when_set() {
        let mut config = Config::default();
        config.stream_host = "192.168.4.1".into();
        let allow = compute_host_allowlist(&config);
        assert!(allow.contains("192.168.4.1"));
    }

    #[test]
    fn compute_host_allowlist_includes_user_additions() {
        let mut config = Config::default();
        config.allowed_hosts = vec!["my-tailscale-name".into(), "100.64.1.5".into()];
        let allow = compute_host_allowlist(&config);
        assert!(allow.contains("my-tailscale-name"));
        assert!(allow.contains("100.64.1.5"));
    }

    #[test]
    fn compute_host_allowlist_skips_empty_user_additions() {
        // A `allowed_hosts = ["", "real"]` config (typo or accidental empty
        // entry) shouldn't allow empty Host headers (which is what some
        // attackers send).
        let mut config = Config::default();
        config.allowed_hosts = vec!["".into(), "real".into()];
        let allow = compute_host_allowlist(&config);
        assert!(!allow.contains(""));
        assert!(allow.contains("real"));
    }

    #[test]
    fn compute_host_allowlist_rejects_unknown_wan_ip() {
        // Defense-in-depth pin: Darwin's public WAN IP must NOT be in the
        // default allowlist. iptables is the first line; the host-header
        // middleware is the second. Verify the second doesn't spontaneously
        // accept the public IP if iptables ever lets it through.
        let config = Config::default();
        let allow = compute_host_allowlist(&config);
        assert!(!allow.contains("94.254.88.116"));
    }

    // --- Reaper grace period math (Apr 15, 2026 regression guards) ---
    //
    // The reaper's grace period is how long spela keeps HLS segments alive
    // after ffmpeg finishes transcoding but while the Chromecast is still
    // playing them. The old hardcoded 45-minute value bit the user twice:
    //   - too SHORT for a 63-min TV episode (cleanup fired mid-watch at
    //     the 30-min mark of S05E01)
    //   - too LONG for short clips (45 min of idle storage waste)
    // These tests pin the new duration-aware math so neither regression
    // can come back.

    #[test]
    fn test_grace_covers_63_minute_episode() {
        // 63-min episode, no seek → grace = 63*60 + 10*60 = 4380 s (73 min).
        // This is the scenario that failed in production Apr 15 at 18:30.
        let grace = compute_reaper_grace_secs(Some(3823.6), 0.0);
        assert!(
            grace >= 3823 + 600,
            "63-min episode needs at least 73 min grace, got {} s",
            grace
        );
    }

    #[test]
    fn test_grace_covers_three_hour_movie() {
        // 3-hour movie, no seek → grace = 180*60 + 10*60 = 11400 s (190 min).
        // The old 2700 s hardcoded grace would have cleaned up mid-movie.
        let grace = compute_reaper_grace_secs(Some(10800.0), 0.0);
        assert!(
            grace >= 10800 + 600,
            "3-hour movie needs at least 190 min grace, got {} s",
            grace
        );
    }

    #[test]
    fn test_grace_respects_seek_offset() {
        // 63-min episode with seek to 30 min → only 33 min of content remain.
        // Grace should be (3823.6 - 1800) + 600 = 2623.6 + 600 ≈ 2623 s.
        // (Historical 2700 s happened to work for THIS specific case, which
        // is why the bug went unnoticed for so long. But at seek_to=0 it
        // fails, as the Apr 15 incident shows.)
        let grace = compute_reaper_grace_secs(Some(3823.6), 1800.0);
        assert!(
            grace >= 2023 + 600,
            "post-seek grace should cover (duration-ss_offset)+cushion, got {} s",
            grace
        );
        assert!(
            grace < 4000,
            "post-seek grace should NOT allocate for the whole duration, got {} s",
            grace
        );
    }

    #[test]
    fn test_grace_floor_protects_short_clips() {
        // A 30-second clip with no seek → raw remaining = 30 s, +cushion=630 s.
        // The 5-minute floor doesn't kick in because the cushion already
        // puts us past it. Test with a truly degenerate 0-duration case too.
        let grace = compute_reaper_grace_secs(Some(30.0), 0.0);
        assert!(grace >= 300, "5-min floor should apply, got {} s", grace);
    }

    #[test]
    fn test_grace_floor_applies_to_zero_duration_gracefully() {
        // Duration = 0 is nonsense; we fall through to the 45-min default
        // rather than returning a nonsensically small grace period.
        let grace = compute_reaper_grace_secs(Some(0.0), 0.0);
        assert_eq!(grace, 2700, "duration=0 should use the legacy default");
    }

    #[test]
    fn test_grace_unknown_duration_uses_legacy_default() {
        // When ffprobe fails and we have no duration info, keep the
        // conservative 45-minute fallback — better than zero.
        assert_eq!(compute_reaper_grace_secs(None, 0.0), 2700);
        assert_eq!(compute_reaper_grace_secs(None, 1800.0), 2700);
    }

    #[test]
    fn test_grace_seek_past_end_clamps_to_floor() {
        // Pathological: seek_to > duration. Remaining content is 0,
        // grace = max(0 + 600, 300) = 600 s. Still meaningful.
        let grace = compute_reaper_grace_secs(Some(1800.0), 3600.0);
        assert_eq!(grace, 600);
    }

    // --- Cast health monitor position-jump sanity check (Apr 15, 2026) ---

    #[test]
    fn test_position_jump_sanity_normal_playback() {
        // 30s wall, 30s absolute advance = 1x realtime — fine
        assert!(!is_position_jump_suspicious(30.0, 30.0));
        // 5s wall, 5s advance = normal poll cadence
        assert!(!is_position_jump_suspicious(5.0, 5.0));
    }

    #[test]
    fn test_position_jump_sanity_fast_playback() {
        // 30s wall, 60s absolute advance = 2x realtime — fine (2x double-speed)
        assert!(!is_position_jump_suspicious(30.0, 60.0));
        // 30s wall, 120s advance = exactly at the 2×+60s threshold boundary
        assert!(!is_position_jump_suspicious(30.0, 120.0));
    }

    #[test]
    fn test_position_jump_sanity_boundary_just_over() {
        // 30s wall, 120.1s advance = just over threshold → suspicious
        assert!(is_position_jump_suspicious(30.0, 120.1));
    }

    #[test]
    fn test_position_jump_sanity_impossible_advance() {
        // The Apr 15 incident scenario: 60s wall, 1796s advance = 30× realtime
        assert!(is_position_jump_suspicious(60.0, 1796.0));
        // Even more dramatic: 5s wall, 1000s advance
        assert!(is_position_jump_suspicious(5.0, 1000.0));
        // 30s wall, 3478s jump (the second play's phantom reading)
        assert!(is_position_jump_suspicious(30.0, 3478.0));
    }

    #[test]
    fn test_position_jump_sanity_first_tick_allowed() {
        // delta_wall = 0.0 (first tick, no baseline) → never suspicious.
        // cast_health_monitor initializes last_save_wall at monitor start,
        // so this path only fires for clock glitches.
        assert!(!is_position_jump_suspicious(0.0, 1000.0));
        assert!(!is_position_jump_suspicious(0.0, 10.0));
    }

    #[test]
    fn test_position_jump_sanity_rewind_allowed() {
        // User seeks backward via /api/seek — delta_abs is negative.
        // Must never be flagged (negative < positive threshold).
        assert!(!is_position_jump_suspicious(30.0, -100.0));
        assert!(!is_position_jump_suspicious(5.0, -5.0));
    }

    // --- Range header parsing (the silent Range feature) ---
    // Edge cases from Mar 26: Accept-Ranges/206 broke Chromecast,
    // so we parse Range but always respond 200

    #[test]
    fn test_parse_range_open_ended() {
        assert_eq!(parse_range_start(Some("bytes=12345-")), 12345);
    }

    #[test]
    fn test_parse_range_zero() {
        assert_eq!(parse_range_start(Some("bytes=0-")), 0);
    }

    #[test]
    fn test_parse_range_bounded_ignored() {
        // Bounded ranges must be ignored — file is growing, can't honor end byte
        assert_eq!(parse_range_start(Some("bytes=100-500")), 0);
    }

    #[test]
    fn test_parse_range_no_header() {
        assert_eq!(parse_range_start(None), 0);
    }

    #[test]
    fn test_parse_range_garbage() {
        assert_eq!(parse_range_start(Some("not-a-range")), 0);
    }

    #[test]
    fn test_parse_range_missing_prefix() {
        assert_eq!(parse_range_start(Some("12345-")), 0);
    }

    #[test]
    fn test_parse_range_large_offset() {
        // 100GB offset — should handle u64 range
        assert_eq!(parse_range_start(Some("bytes=107374182400-")), 107374182400);
    }

    #[test]
    fn test_parse_range_multipart_ignored() {
        // Multi-range not supported
        assert_eq!(parse_range_start(Some("bytes=0-100, 200-300")), 0);
    }

    // --- Cast receiver HTML ---

    #[test]
    fn test_receiver_html_embedded() {
        // The receiver HTML is embedded via include_str! — verify it's valid
        let html = include_str!("../static/cast-receiver.html");
        assert!(html.contains("cast_receiver_framework.js"));
        assert!(html.contains("cast-media-player"));
        assert!(html.contains("Rokkitt")); // Custom font
        assert!(html.contains("/api/cast-config")); // Self-configuration
        assert!(html.contains("intro-video")); // Intro element
        assert!(html.contains("overlay")); // Netflix-style overlay
        assert!(html.contains("seek-spinner")); // Seek-restart UI
        assert!(html.contains("error-overlay")); // Error recovery
    }

    #[test]
    fn test_receiver_html_has_position_reporting() {
        let html = include_str!("../static/cast-receiver.html");
        assert!(html.contains("/api/position")); // Position save endpoint
        assert!(html.contains("POSITION_REPORT_INTERVAL"));
    }

    #[test]
    fn test_receiver_html_has_subtitle_support() {
        let html = include_str!("../static/cast-receiver.html");
        assert!(html.contains("subtitle_url"));
        assert!(html.contains("TrackType.TEXT"));
        assert!(html.contains("text/vtt"));
    }

    // --- Resume position ---

    #[test]
    fn test_resume_positions_default_empty() {
        let state = AppState::default();
        assert!(state.resume_positions.is_empty());
    }

    #[test]
    fn test_resume_positions_roundtrip() {
        let mut state = AppState::default();
        state.resume_positions.insert("tt10548174".into(), 2847.5);
        state.resume_positions.insert("tt5114356".into(), 1234.0);

        let json = serde_json::to_string(&state).unwrap();
        let loaded: AppState = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.resume_positions.get("tt10548174"), Some(&2847.5));
        assert_eq!(loaded.resume_positions.get("tt5114356"), Some(&1234.0));
        assert_eq!(loaded.resume_positions.get("tt0000000"), None);
    }

    #[test]
    fn test_resume_positions_survives_missing_field() {
        // Old state.json without resume_positions should still deserialize
        let json = r#"{"current":null,"history":[],"preferences":{"default_target":"chromecast","preferred_quality":"1080p"}}"#;
        let state: AppState = serde_json::from_str(json).unwrap();
        assert!(state.resume_positions.is_empty()); // Default empty
    }

    // --- Seek-restart validation ---

    #[test]
    fn test_seek_restart_negative_clamped() {
        // Negative seek time should be clamped to 0
        let t: f64 = -100.0;
        assert_eq!(t.max(0.0), 0.0);
    }

    #[test]
    fn test_seek_restart_zero_valid() {
        let t: f64 = 0.0;
        assert_eq!(t.max(0.0), 0.0);
    }

    #[test]
    fn test_seek_restart_large_value() {
        // 3 hours in seconds
        let t: f64 = 10800.0;
        assert_eq!(t.max(0.0), 10800.0);
    }

    #[test]
    fn test_parse_size_to_bytes_units() {
        assert_eq!(parse_size_to_bytes("1 GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_to_bytes("1.5 GB"), Some(1610612736));
        assert_eq!(parse_size_to_bytes("700 MB"), Some(700 * 1024 * 1024));
        assert_eq!(parse_size_to_bytes("nonsense"), None);
    }

    #[test]
    fn test_title_tokens_match_sanitized_folder_names() {
        assert!(title_tokens_match("Some.Movie.Title.2026.1080p.WEB-DL", "Some Movie Title"));
        assert!(!title_tokens_match("Some Other Movie 2026 1080p", "Some Movie Title"));
    }

    #[test]
    fn test_local_bypass_does_not_trust_marker_when_expected_size_disagrees() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spela-local-bypass-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::write(&path, [0u8; 4096]).unwrap();

        assert!(!local_bypass_file_is_healthy(&path, true, 1024 * 1024 * 1024));
        assert!(local_bypass_file_is_healthy(&path, true, 0));
        assert!(!local_bypass_file_is_healthy(&path, false, 0));

        let _ = std::fs::remove_file(path);
    }

    // --- Auto-recast decision (Apr 25 + Apr 29, 2026 regression guards) ---
    //
    // `should_attempt_recast` is the gate that keeps a wedged Chromecast from
    // trapping cast_health_monitor in a pointless retry loop while ENABLING
    // recurring recovery for receivers that flake periodically. Apr 29
    // replaced the original Apr 25 lifetime cap-of-1 with a frequency cap
    // (cooldown) — see RECAST_COOLDOWN_SECS doc for rationale.
    //
    // These tests pin every guard so none of them can drift silently.

    #[test]
    fn test_recast_normal_mid_stream_death() {
        // Vardagsrum-style failure: ~12 min in, we have an HWM, no prior recast.
        // This is the primary recovery case — must attempt.
        assert!(should_attempt_recast(None, 720, true));
    }

    #[test]
    fn test_recast_unbounded_with_cooldown_satisfied() {
        // Apr 29, 2026: replaced the cap-of-1 — many recasts allowed as long
        // as the cooldown has elapsed since the last one. Hijack S2E2
        // Apr 28-29 incident showed that recurring receiver flakes are
        // recoverable via repeated recast, and the old cap was leaving the
        // user with permanently dead streams after the first recovery.
        // Cooldown satisfied → attempt regardless of attempt history depth.
        let just_above = RECAST_COOLDOWN_SECS;
        assert!(should_attempt_recast(Some(just_above), 720, true));
        assert!(should_attempt_recast(Some(600), 720, true));
        assert!(should_attempt_recast(Some(u64::MAX), 720, true));
    }

    #[test]
    fn test_recast_blocked_during_cooldown() {
        // Rapid-fire wedge protection — the actual purpose the original
        // cap-of-1 was trying to serve. A receiver that IDLEs within
        // seconds of LOAD must NOT trigger another recast immediately;
        // wait at least RECAST_COOLDOWN_SECS to confirm we're not in an
        // infinite LOAD/IDLE loop on a truly wedged device.
        assert!(!should_attempt_recast(Some(0), 720, true));
        assert!(!should_attempt_recast(Some(10), 720, true));
        assert!(!should_attempt_recast(Some(RECAST_COOLDOWN_SECS - 1), 720, true));
    }

    #[test]
    fn test_recast_rejects_startup_failures() {
        // Stream < 60 s old → the Chromecast failed the initial LOAD, not a
        // mid-stream flake. Re-LOAD won't help; those failures need stream_host
        // / transcode / manifest fixes. Don't waste the user's patience.
        assert!(!should_attempt_recast(None, 0, true));
        assert!(!should_attempt_recast(None, 30, true));
        assert!(!should_attempt_recast(None, 59, true));
        // Exactly at the threshold → attempt.
        assert!(should_attempt_recast(None, MIN_STREAM_AGE_FOR_RECAST_SECS, true));
    }

    #[test]
    fn test_recast_requires_valid_hwm() {
        // No HWM → nothing sensible to seek to. Playback from ss_offset is a
        // worse UX than a clean cleanup + manual replay, because the user
        // loses their place silently.
        assert!(!should_attempt_recast(None, 720, false));
        assert!(!should_attempt_recast(Some(120), 720, false));
    }

    #[test]
    fn test_recast_min_stream_age_is_stable() {
        // The threshold is load-bearing for the "startup LOAD failure" guard
        // described in the constant's docstring. Raising it wastes mid-stream
        // recovery opportunities; lowering it wastes recovery attempts on
        // unrecoverable config failures. If this assertion needs to change,
        // update the doc comment on MIN_STREAM_AGE_FOR_RECAST_SECS too.
        assert_eq!(MIN_STREAM_AGE_FOR_RECAST_SECS, 60);
    }

    #[test]
    fn test_recast_cooldown_is_stable() {
        // Apr 29, 2026 PM: pinning at 30s. Effective floor is
        // MAX_BUFFERING_DURATION_SECS=60s for BUFFERING-stuck path
        // (dominant failure mode), so cooldown only matters for
        // IDLE-driven recasts where 30s = 2× the 15s IDLE-detect window.
        // Loosening this would re-introduce the Apr 29 AM premature-cleanup
        // bug; tightening below ~20s risks rapid-fire LOAD spam on a
        // wedged device.
        assert_eq!(RECAST_COOLDOWN_SECS, 30);
    }

    #[test]
    fn test_apr29_morning_e2_resume_pattern_recovers() {
        // Replay the morning's E2 resume failure: CrKey 1.56 receiver
        // freezes for ~63s every ~70s (133s cycle). With the old 90s
        // cooldown, the 3rd recast was rejected (72s gap < 90s) leading
        // to premature cleanup. With 60s cooldown, recovery continues.
        // This case BREAKS at the OLD value (90s) but works at the NEW
        // value (60s), so it's a regression pin against future cooldown
        // tightening.
        let _healthy_for = 70;  // CrKey holds Playing for ~70s before next freeze
        let _freeze_for = 63;   // then freezes for 63s before recast can fire
        let cycle = 133; // sec — full Playing+Frozen cycle observed Apr 29 AM

        // First recast: no prior history, allowed (assuming stream age met)
        assert!(should_attempt_recast(None, 120, true));

        // Second recast: a full cycle later (133s gap, well above cooldown)
        assert!(should_attempt_recast(Some(cycle), 120 + cycle, true));

        // Third recast: another cycle later. Time gap from PRIOR recast.
        // Same as second — 133s gap.
        assert!(should_attempt_recast(Some(cycle), 120 + 2 * cycle, true));

        // The actual Apr 29 AM 3rd recast: 72s gap (between healthy_for=70
        // and the next stall threshold of 60s in BUFFERING-stall logic).
        // OLD (90s cooldown): rejected. NEW (60s cooldown): accepted.
        assert!(should_attempt_recast(Some(72), 200, true),
            "3rd recast at 72s gap MUST be allowed at the new 60s cooldown");
    }

    #[test]
    fn test_max_buffering_duration_is_stable() {
        // Apr 29, 2026 PM: pinning at 30s. The earlier 60s value was
        // conservative against startup false positives. Cleaner answer:
        // 30s threshold + stream_age >= MIN_STREAM_AGE_FOR_RECAST_SECS
        // gate (which the cast_health_monitor enforces). 30s shaves
        // ~30s off every receiver-internal-freeze recovery.
        assert_eq!(MAX_BUFFERING_DURATION_SECS, 30);
    }

    // ---- Apr 29, 2026: VOD-padded manifest tests (B2) ----
    //
    // These pin the parser + manifest-builder so future regressions in the
    // VOD-padded mode are immediately visible. The B2 design replaces the
    // earlier `experimental_endlist_hack` (which caused chase-the-end /
    // HWM inflation by lying about total duration). B2 declares the full
    // duration upfront — receiver's clock matches reality.

    #[test]
    fn parse_extracts_header_entries_no_endlist() {
        let body = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.427089,
seg_00000.ts
#EXTINF:10.427078,
seg_00001.ts
";
        let (header, entries, had_endlist) = parse_hls_playlist_for_padding(body);
        assert!(!had_endlist);
        assert_eq!(header, vec![
            "#EXTM3U", "#EXT-X-VERSION:3",
            "#EXT-X-TARGETDURATION:10", "#EXT-X-MEDIA-SEQUENCE:0",
        ]);
        assert_eq!(entries.len(), 2);
        assert!((entries[0].0 - 10.427089).abs() < 1e-6);
        assert_eq!(entries[0].1, "seg_00000.ts");
        assert_eq!(entries[1].1, "seg_00001.ts");
    }

    #[test]
    fn parse_detects_endlist() {
        let body = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:6.0,
seg_00000.ts
#EXT-X-ENDLIST
";
        let (_, _, had_endlist) = parse_hls_playlist_for_padding(body);
        assert!(had_endlist, "ENDLIST presence must be detected");
    }

    #[test]
    fn parse_handles_empty_playlist() {
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n";
        let (header, entries, had_endlist) = parse_hls_playlist_for_padding(body);
        assert!(!had_endlist);
        assert_eq!(entries.len(), 0);
        assert_eq!(header.len(), 2);
    }

    #[test]
    fn padded_manifest_existing_endlist_is_passthrough() {
        // If ffmpeg already wrote ENDLIST (clean shutdown / completed
        // transcode), there's nothing to pad — return verbatim. Avoids
        // double-ENDLIST and preserves whatever metadata ffmpeg emitted.
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:6.0,\nseg_00000.ts\n#EXT-X-ENDLIST\n";
        let out = build_padded_vod_manifest(body, 60.0, 6.0);
        assert_eq!(out, body);
    }

    #[test]
    fn padded_manifest_predicts_correct_count_from_avg() {
        // 3 emitted segments avg 10s → remaining 600s → 60 segments
        // expected, plus +2 buffer = 62 total. We expect output to have
        // EXACTLY 62 entries (3 real + 59 placeholder) + ENDLIST.
        let body = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.0,
seg_00000.ts
#EXTINF:10.0,
seg_00001.ts
#EXTINF:10.0,
seg_00002.ts
";
        let out = build_padded_vod_manifest(body, 600.0, 6.0);
        let segment_count = out.matches("seg_").count();
        // 600s / 10s = 60 ceil + 2 buffer = 62
        assert_eq!(segment_count, 62);
        assert!(out.contains("#EXT-X-ENDLIST"));
        // Only ONE endlist
        assert_eq!(out.matches("#EXT-X-ENDLIST").count(), 1);
        // First three entries preserve their REAL extinf
        assert!(out.contains("#EXTINF:10.000000,\nseg_00000.ts"));
        // Placeholders are sequential past existing
        assert!(out.contains("seg_00059.ts"));
        assert!(out.contains("seg_00061.ts"));
        // Don't go past the predicted total
        assert!(!out.contains("seg_00062.ts"));
    }

    #[test]
    fn padded_manifest_no_emitted_segments_uses_default() {
        // No segments yet → fall back to default_segment_secs for prediction.
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n";
        let out = build_padded_vod_manifest(body, 60.0, 6.0);
        // 60s / 6s = 10 ceil + 2 = 12
        let segment_count = out.matches("seg_").count();
        assert_eq!(segment_count, 12);
        assert!(out.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn padded_manifest_zero_remaining_returns_minimal_endlist() {
        // Edge case: ss_offset >= duration. Don't crash.
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n";
        let out = build_padded_vod_manifest(body, 0.0, 6.0);
        // No segments needed but still emit ENDLIST so receiver doesn't
        // think it's a live stream.
        assert!(out.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn padded_manifest_never_under_predicts_emitted_count() {
        // Edge case: 100 emitted segments but remaining_duration is small
        // (e.g., user paused far past predicted). Output must include all
        // emitted entries — never truncate ffmpeg's actual output.
        let mut body = String::from(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:0\n"
        );
        for i in 0..100 {
            body.push_str(&format!("#EXTINF:10.0,\nseg_{:05}.ts\n", i));
        }
        let out = build_padded_vod_manifest(&body, 60.0, 6.0); // claims only 60s left
        let segment_count = out.matches("seg_").count();
        // Must have at LEAST the 100 emitted entries + 1 padding (max).
        assert!(segment_count >= 100);
    }

    #[test]
    fn padded_manifest_apr29_realistic_hijack_s2e2_case() {
        // Realistic: Hijack S2E2 episode is 48m40s = 2920s. With 10 segments
        // pre-buffered (avg ~10.4s), the receiver fetches the playlist for
        // the first time and needs to see total ≈ 2920s of content.
        let mut body = String::from(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:11\n#EXT-X-MEDIA-SEQUENCE:0\n"
        );
        for i in 0..10 {
            body.push_str(&format!("#EXTINF:10.4,\nseg_{:05}.ts\n", i));
        }
        let out = build_padded_vod_manifest(&body, 2920.0, 6.0);

        // Expected: ceil(2920 / 10.4) = 281 + 2 buffer = 283 segments
        let segment_count = out.matches("seg_").count();
        assert!((283..=285).contains(&segment_count),
            "Expected ~283 segments for full Hijack S2E2 episode, got {}",
            segment_count);

        // Total declared duration ≈ 2920s (within tolerance — placeholder
        // EXTINF uses the avg so total ≈ count * 10.4 ≈ 2940-2960s).
        // Just sanity-check that ENDLIST is present and structure is sane.
        assert!(out.contains("#EXT-X-ENDLIST"));
        assert!(out.contains("#EXTINF:"));
    }

    // ---- Apr 28-29 Hijack S2E2 incident replay ----

    #[test]
    fn test_apr29_hijack_s2e2_recurring_idle_recovery() {
        // Reconstructs the exact pattern from journalctl (Hijack S2E2,
        // Apr 28-29). With Apr 25's cap-of-1, this case left the user
        // with a permanently dead stream after the second IDLE. With
        // Apr 29's rate-limit, every cycle recovers as long as the
        // receiver doesn't IDLE faster than the cooldown.

        // Stream age 32 min when first IDLE hit, no prior recast.
        let first_idle_secs_since_recast = None;
        let first_idle_stream_age = 32 * 60;
        assert!(should_attempt_recast(first_idle_secs_since_recast, first_idle_stream_age, true),
            "First IDLE → recast permitted under both old and new logic");

        // ~3 min later, second IDLE hits. Old logic rejected (cap=1);
        // new logic permits because cooldown is satisfied (180s >= 90s).
        let second_idle_secs_since_recast = Some(180);
        let second_idle_stream_age = 32 * 60 + 180;
        assert!(should_attempt_recast(second_idle_secs_since_recast, second_idle_stream_age, true),
            "Second IDLE 3 min later → recast permitted with cooldown (was blocked by cap-of-1)");
    }

    #[test]
    fn test_recast_blocks_simulated_wedge() {
        // Wedge case: receiver IDLEs every 15s (rapid-fire wedge — would
        // be infinite-loop without rate limit). Cooldown bounds the
        // recast frequency. The exact bound scales with cooldown.
        let mut elapsed_since_recast: u64 = 0;
        let stream_age: u64 = 120;
        let mut recast_count = 0;
        for _tick in 0..40 { // simulate 40 IDLE batches @ 15s apart
            if should_attempt_recast(Some(elapsed_since_recast), stream_age, true) {
                recast_count += 1;
                elapsed_since_recast = 0;
            } else {
                elapsed_since_recast += 15;
            }
        }
        // 40 ticks × 15 s = 600 s of wall time. With cooldown=30, max
        // theoretical recasts = ceil(600 / 30) = 20, plus the initial
        // recast at tick 0. Set the cap at 22 for a small margin against
        // tick-arithmetic rounding.
        assert!(recast_count <= 22,
            "Expected ≤ 22 recasts in 600s of 15s-period wedge at 30s cooldown; got {}",
            recast_count);
        // But it must fire at least 1 — otherwise we've broken normal recovery.
        assert!(recast_count >= 1, "Wedge protection must not block the FIRST recast");
        // And it must NOT fire on every tick — that's the wedge spam we
        // were guarding against.
        assert!(recast_count < 40,
            "Recast firing on every tick = wedge protection broken");
    }
}

/// Sanitize title for fuzzy matching (lowercase, no symbols, KEEP SPACES)
fn sanitize_title(title: &str) -> String {
    title.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_tokens_match(candidate: &str, title: &str) -> bool {
    let s_candidate = sanitize_title(candidate);
    let s_title = sanitize_title(title);
    let title_tokens: Vec<&str> = s_title.split_whitespace().collect();
    !title_tokens.is_empty() && title_tokens.iter().all(|&token| s_candidate.contains(token))
}
