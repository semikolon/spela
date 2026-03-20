use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::Method;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::cast::CastController;
use crate::config::Config;
use crate::disk;
use crate::search::SearchEngine;
use crate::state::{AppState, CurrentStream};
use crate::subtitles;
use crate::torrent;
use crate::transcode;

pub struct ServerState {
    pub config: Config,
    pub search_engine: SearchEngine,
    pub cast: Mutex<CastController>,
    pub state_dir: PathBuf,
    pub media_dir: PathBuf,
    /// PID of the running ffmpeg transcode process (if any)
    pub ffmpeg_pid: Mutex<Option<u32>>,
}

type SharedState = Arc<ServerState>;

pub async fn run_server(config: Config) -> anyhow::Result<()> {
    let state_dir = Config::state_dir();
    let media_dir = config.media_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&media_dir)?;

    let search_engine = SearchEngine::new(config.tmdb_api_key.clone());
    let cast = Mutex::new(CastController::new(&state_dir));
    let port = config.port;

    let state = Arc::new(ServerState {
        config,
        search_engine,
        cast,
        state_dir,
        media_dir,
        ffmpeg_pid: Mutex::new(None),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
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
        .route("/config", get(handle_get_config).post(handle_set_config))
        .route("/cast-info", post(handle_cast_info))
        .route("/stream/transcode", get(handle_transcode_stream))
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("spela server listening on http://{}", addr);
    tracing::info!("Endpoints: /search /play /stop /status /pause /resume /seek /volume /next /prev /targets /history /config");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
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
                            // Clean up partial files from failed attempt
                            let transcoded = state.media_dir.join("transcoded_aac.mp4");
                            if transcoded.exists() {
                                let _ = std::fs::remove_file(&transcoded);
                            }
                            if let Some(pid) = state.ffmpeg_pid.lock().unwrap().take() {
                                torrent::kill_pid(pid);
                            }
                            req.result_id = Some(next_rid);
                            req.magnet = None;
                            req.file_index = None;
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

    // Disk check
    if let Ok(Some(err)) = disk::check_space(&state.media_dir) {
        return Json(json!({"error": err}));
    }
    disk::cleanup_old_files(&state.media_dir);

    // Stop existing stream
    let pid_path = state.state_dir.join("webtorrent.pid");
    torrent::stop_by_pid_file(&pid_path);

    let mut app_state = AppState::load(&state.state_dir);
    app_state.stop_current();

    let target = req.target.as_deref().unwrap_or(&app_state.preferences.default_target).to_string();
    let cast_name = req.cast_name.clone()
        .or_else(|| app_state.preferences.chromecast_name.clone())
        .unwrap_or_else(|| state.config.default_device.clone());
    let no_subs = req.no_subs.unwrap_or(false);
    let sub_lang = req.subtitle_lang.clone().unwrap_or_else(|| "eng".into());

    // Start webtorrent
    let log_path = state.state_dir.join("webtorrent.log");
    let (pid, server_url) = match torrent::start_webtorrent(
        &magnet, req.file_index, &state.media_dir, &state.config.lan_ip, &log_path
    ).await {
        Ok(r) => r,
        Err(e) => return Json(json!({"error": e.to_string()})),
    };

    let _ = torrent::save_pid(&state.state_dir.join("webtorrent.pid"), pid);

    // Self-healing: check download progress — if 0% after 12s, torrent has dead seeds
    if !torrent::check_progress(&log_path, 12).await {
        tracing::warn!("Torrent has no download progress after 12s — dead seeds");
        torrent::kill_pid(pid);
        torrent::kill_all_webtorrent();
        disk::cleanup_old_files(&state.media_dir);
        return Json(json!({"error": "Torrent has no active seeds (0% after 12s)"}));
    }

    // Fetch subtitles FIRST (needed for burn-in during transcode)
    let mut has_subtitles = false;
    let mut subtitle_srt_path: Option<PathBuf> = None;
    if !no_subs {
        if let Some(imdb_id) = &req.imdb_id {
            let client = reqwest::Client::new();
            match subtitles::fetch_subtitles(&client, imdb_id, req.season, req.episode, &sub_lang, &state.media_dir).await {
                Ok(Some(vtt_path)) => {
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

    // Audio codec detection + transcode (reads from HTTP with -re, never outruns download)
    let mut final_url = server_url.clone();
    let mut is_transcoded = false;
    let no_intro = req.no_intro.unwrap_or(false);
    let intro_path = if no_intro { None } else { transcode::find_intro() };
    match transcode::detect_audio_codec(&server_url).await {
        Ok(Some(codec)) if transcode::needs_transcode(&codec) => {
            tracing::info!("Audio codec {} needs transcode -> AAC{}{}",
                codec,
                if subtitle_srt_path.is_some() { " + subtitle burn-in (NVENC)" } else { "" },
                if intro_path.is_some() { " + intro clip" } else { "" });

            let sub_path = subtitle_srt_path.as_deref();
            match transcode::transcode_audio(&server_url, &state.media_dir, sub_path, intro_path.as_deref()).await {
                Ok((output_path, ffmpeg_pid)) => {
                    // Track ffmpeg PID for the streaming endpoint and cleanup
                    *state.ffmpeg_pid.lock().unwrap() = Some(ffmpeg_pid);

                    // Wait for sufficient buffer before casting.
                    // 5MB proves sustained torrent download + transcode pipeline health.
                    // Intro concat + NVENC re-encoding needs more time (~30s) than
                    // simple audio transcode with video copy (~14s).
                    let prebuffer_min: u64 = 5 * 1024 * 1024; // 5MB
                    let timeout_secs = if intro_path.is_some() { 45 } else { 25 };
                    let prebuffer_deadline = tokio::time::Instant::now()
                        + tokio::time::Duration::from_secs(timeout_secs);
                    loop {
                        if tokio::time::Instant::now() > prebuffer_deadline {
                            tracing::warn!("Pre-buffer timeout ({}s) — casting with available data", timeout_secs);
                            break;
                        }
                        if let Ok(meta) = std::fs::metadata(&output_path) {
                            if meta.len() >= prebuffer_min {
                                tracing::info!("Pre-buffer ready: {}KB", meta.len() / 1024);
                                break;
                            }
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }

                    // Serve via axum's streaming endpoint (chunked transfer, no Content-Length)
                    // This replaces the python http.server which sent Content-Length for a growing file
                    final_url = format!("http://{}:{}/stream/transcode", state.config.lan_ip, state.config.port);
                    is_transcoded = true;

                    if sub_path.is_some() {
                        tracing::info!("Subtitles burned into video stream via NVENC");
                    }
                }
                Err(e) => tracing::warn!("Transcode failed (casting original): {}", e),
            }
        }
        _ => {}
    }

    // Cast to Chromecast
    if target == "chromecast" {
        let state_clone = state.clone();
        let cast_name_clone = cast_name.clone();
        let url_clone = final_url.clone();
        let live = is_transcoded;
        let cast_result = tokio::task::spawn_blocking(move || {
            let mut cast = state_clone.cast.lock().unwrap();
            cast.cast_url(&cast_name_clone, &url_clone, "video/mp4", live)
        }).await;

        match cast_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return Json(json!({
                    "error": format!("Cast failed: {}", e),
                    "url": final_url,
                    "recovery_suggestion": "Try 'spela targets' to discover devices, or check if TV is on"
                }));
            }
            Err(e) => return Json(json!({"error": format!("Cast task failed: {}", e)})),
        }

        // Seek to saved position if resuming
        if let Some(seek_to) = req.seek_to {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let state_clone = state.clone();
            let cast_name_clone = cast_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut cast = state_clone.cast.lock().unwrap();
                cast.seek(&cast_name_clone, seek_to)
            }).await;
        }
    }

    // Save state
    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());
    app_state.current = Some(CurrentStream {
        magnet: magnet.chars().take(300).collect(),
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
    });
    let _ = app_state.save(&state.state_dir);

    Json(json!({
        "status": "streaming",
        "pid": pid,
        "target": format!("{}:{}", target, cast_name),
        "title": title,
        "subtitles": has_subtitles,
        "url": final_url
    }))
}

async fn handle_stop(State(state): State<SharedState>) -> Json<Value> {
    let pid_path = state.state_dir.join("webtorrent.pid");
    torrent::stop_by_pid_file(&pid_path);

    // Kill ffmpeg transcode process if running
    if let Some(pid) = state.ffmpeg_pid.lock().unwrap().take() {
        torrent::kill_pid(pid);
        tracing::info!("Killed ffmpeg transcode (PID {})", pid);
    }
    // Kill any lingering python http servers (legacy cleanup)
    let _ = std::process::Command::new("pkill")
        .args(["-f", "python3 -m http.server 8889"])
        .output();

    // Clean up transcoded file
    let transcoded = state.media_dir.join("transcoded_aac.mp4");
    if transcoded.exists() {
        let _ = std::fs::remove_file(&transcoded);
    }

    let mut app_state = AppState::load(&state.state_dir);
    app_state.stop_current();
    let _ = app_state.save(&state.state_dir);

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
        let mut cast = state_clone.cast.lock().unwrap();
        cast.pause(&device)
    }).await;
    cast_result_to_json(result)
}

async fn handle_resume(State(state): State<SharedState>) -> Json<Value> {
    let device = get_current_device(&state);
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = state_clone.cast.lock().unwrap();
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
        let mut cast = state_clone.cast.lock().unwrap();
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
        let mut cast = state_clone.cast.lock().unwrap();
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
    };

    handle_play(State(state.clone()), Json(play_req)).await
}

async fn handle_targets(State(state): State<SharedState>) -> Json<Value> {
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = state_clone.cast.lock().unwrap();
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

async fn handle_get_config(State(state): State<SharedState>) -> Json<Value> {
    let app_state = AppState::load(&state.state_dir);
    Json(json!({"preferences": app_state.preferences}))
}

async fn handle_set_config(
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut app_state = AppState::load(&state.state_dir);
    if let Some(obj) = body.as_object() {
        if let Some(v) = obj.get("default_target").and_then(|v| v.as_str()) {
            app_state.preferences.default_target = v.into();
        }
        if let Some(v) = obj.get("chromecast_name").and_then(|v| v.as_str()) {
            app_state.preferences.chromecast_name = Some(v.into());
        }
        if let Some(v) = obj.get("preferred_quality").and_then(|v| v.as_str()) {
            app_state.preferences.preferred_quality = v.into();
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
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut cast = state_clone.cast.lock().unwrap();
        cast.get_info(&device)
    }).await;
    match result {
        Ok(Ok(info)) => Json(serde_json::to_value(info).unwrap_or(json!({"error": "serialize"}))),
        Ok(Err(e)) => Json(json!({"error": e.to_string()})),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

/// Stream the transcoded file with chunked transfer encoding (no Content-Length).
/// This replaces python http.server which sent Content-Length based on file size at request time,
/// causing Chromecast to stop after reading the initial Content-Length bytes of a growing file.
async fn handle_transcode_stream(
    State(state): State<SharedState>,
) -> impl IntoResponse {
    let path = state.media_dir.join("transcoded_aac.mp4");
    let ffmpeg_pid = *state.ffmpeg_pid.lock().unwrap();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

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

        let mut buf = vec![0u8; 64 * 1024]; // 64KB read buffer
        let mut stall_count = 0u32;

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

                    // ffmpeg still running — file will grow, wait and retry
                    stall_count += 1;
                    if stall_count > 600 { // 5 minutes of stall = give up
                        tracing::warn!("Transcode stream stalled for 5 minutes, closing");
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(n) => {
                    stall_count = 0;
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

    axum::response::Response::builder()
        .header("Content-Type", "video/mp4")
        .header("Cache-Control", "no-cache, no-store")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
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
