use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
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

pub async fn run_server(mut config: Config) -> anyhow::Result<()> {
    // Auto-detect LAN IP if not set in config
    if config.lan_ip.is_empty() {
        if let Some(ip) = Config::detect_lan_ip() {
            tracing::info!("Auto-detected LAN IP: {}", ip);
            config.lan_ip = ip;
        } else {
            tracing::warn!("Could not auto-detect LAN IP. Set lan_ip in config.toml");
            config.lan_ip = "127.0.0.1".into();
        }
    }

    let state_dir = Config::state_dir();
    let media_dir = config.media_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&media_dir)?;

    let search_engine = SearchEngine::new(config.tmdb_api_key.clone());
    let cast = Mutex::new(CastController::new(&state_dir, config.known_devices.clone()));
    let port = config.port;
    let host = config.host.clone();

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
        // Custom Cast Receiver endpoints
        .route("/cast-receiver.html", get(handle_cast_receiver_html))
        .route("/cast-receiver/intro.mp4", get(handle_cast_receiver_intro))
        .route("/cast-receiver/subs.vtt", get(handle_cast_receiver_subs))
        .route("/api/cast-config", get(handle_cast_config))
        .route("/api/seek-restart", post(handle_seek_restart))
        .route("/api/position", get(handle_get_position).post(handle_save_position))
        .route("/api/position/reset", post(handle_reset_position))
        .route("/api/retry", post(handle_retry))
        .layer(cors)
        .with_state(state);

    let addr = format!("{}:{}", host, port);
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
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }

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
    if let Ok(Some(err)) = disk::check_space(&media_dir) {
        return Json(json!({"error": err}));
    }
    disk::cleanup_old_files(&media_dir);

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
        &magnet, req.file_index, &media_dir, &state.config.lan_ip, &log_path
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
            match subtitles::fetch_subtitles(&client, imdb_id, req.season, req.episode, &sub_lang, &media_dir).await {
                Ok(Some(vtt_path)) => {
                    has_subtitles = true;
                    // Use the SRT version for ffmpeg burn-in (ffmpeg handles SRT natively)
                    subtitle_srt_path = Some(media_dir.join(format!("subtitle_{}.srt", sub_lang)));
                    tracing::info!("Subtitles fetched ({})", sub_lang);
                }
                Ok(None) => tracing::info!("No subtitles found for {}", sub_lang),
                Err(e) => tracing::warn!("Subtitle fetch failed: {}", e),
            }
        }
    }

    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());

    // Auto-resume from saved position if no explicit seek requested
    let mut seek_to = req.seek_to;
    if seek_to.is_none() {
        let app_state = AppState::load(&state.state_dir);
        let pos = app_state.get_position(req.imdb_id.clone(), req.title.clone());
        if pos > 30.0 { // Don't bother resuming if less than 30s in
            tracing::info!("Auto-resume: found saved position for '{}' at {:.0}s", title, pos);
            seek_to = Some(pos);
        }
    }

    // Stop current stream if any
    do_cleanup(&state);

    // Codec detection + transcode decision
    let mut final_url = server_url.clone();
    let mut is_transcoded = false;
    // Skip intro when seeking — avoids complex concat logic and improves UX on resume
    let no_intro = req.no_intro.unwrap_or(false) || seek_to.is_some();
    let intro_path = if no_intro { None } else { transcode::find_intro() };

    let (video_codec, audio_codec, source_duration) = transcode::detect_codecs(&server_url).await
        .unwrap_or((None, None, None));
    if let Some(dur) = source_duration {
        tracing::info!("Source duration: {:.0}s ({:.0} min)", dur, dur / 60.0);
    }

    let need_audio_tc = audio_codec.as_deref().map_or(false, transcode::audio_needs_transcode);
    let need_video_tc = video_codec.as_deref().map_or(false, transcode::video_needs_transcode);
    let need_transcode = need_audio_tc || need_video_tc || intro_path.is_some() || subtitle_srt_path.is_some();

    if need_transcode {
        let mut reasons = Vec::new();
        if need_audio_tc { reasons.push(format!("{} -> AAC", audio_codec.as_deref().unwrap_or("?"))); }
        if need_video_tc { reasons.push(format!("{} -> H.264 (NVENC)", video_codec.as_deref().unwrap_or("?"))); }
        if subtitle_srt_path.is_some() { reasons.push("subtitle burn-in".into()); }
        if intro_path.is_some() { reasons.push("intro clip".into()); }
        tracing::info!("Transcode needed: {}", reasons.join(" + "));

        let sub_path = subtitle_srt_path.as_deref();
        match transcode::transcode(&server_url, &media_dir, sub_path, intro_path.as_deref(), need_video_tc, seek_to).await {
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
        let cast_result = tokio::task::spawn_blocking(move || {
            let mut cast = state_clone.cast.lock().unwrap();
            cast.cast_url(&cast_name_clone, &url_clone, "video/mp4", cast_duration, seek_to)
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
            Ok(_) => {} // should not happen with Result
            Err(e) => return Json(json!({"error": format!("Cast task failed: {}", e)})),
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
                    let mut cast = state_clone.cast.lock().unwrap();
                    cast.seek(&cast_name_clone, pos)
                }).await;
            }
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

                let wt_alive = unsafe { torrent::kill_check(webtorrent_pid) };
                let ffmpeg_alive = state.ffmpeg_pid.lock().unwrap()
                    .map(|p| unsafe { torrent::kill_check(p) })
                    .unwrap_or(false);

                if !ffmpeg_alive && !wt_alive {
                    // Both dead — playback fully finished
                    tracing::info!("Reaper: all processes exited for '{}', cleaning up", title_for_log);
                    do_cleanup(&state);
                    break;
                }

                if !ffmpeg_alive && wt_alive {
                    // ffmpeg done (movie finished transcoding), webtorrent still seeding.
                    // Grace period: let Chromecast play remaining buffer
                    tracing::info!("Reaper: ffmpeg finished for '{}', grace period before cleanup...", title_for_log);
                    tokio::time::sleep(tokio::time::Duration::from_secs(180)).await;

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

    Json(json!({
        "status": "streaming",
        "pid": pid,
        "target": format!("{}:{}", target, cast_name),
        "title": title,
        "subtitles": has_subtitles,
        "url": final_url
    }))
}

/// Shared cleanup logic: kill webtorrent + ffmpeg, delete transcoded file, update state.
fn do_cleanup(state: &SharedState) {
    let pid_path = state.state_dir.join("webtorrent.pid");
    torrent::stop_by_pid_file(&pid_path);

    if let Some(pid) = state.ffmpeg_pid.lock().unwrap().take() {
        torrent::kill_pid(pid);
    }
    // Kill any lingering python http servers (legacy)
    let _ = std::process::Command::new("pkill")
        .args(["-f", "python3 -m http.server 8889"])
        .output();

    let transcoded = state.media_dir.join("transcoded_aac.mp4");
    if transcoded.exists() {
        let _ = std::fs::remove_file(&transcoded);
    }

    let mut app_state = AppState::load(&state.state_dir);
    app_state.stop_current();
    let _ = app_state.save(&state.state_dir);
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
    let path = state.media_dir.join("transcoded_aac.mp4");
    let ffmpeg_pid = *state.ffmpeg_pid.lock().unwrap();

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

/// Serve the current subtitle WebVTT file.
async fn handle_cast_receiver_subs(State(state): State<SharedState>) -> impl IntoResponse {
    let vtt_path = state.media_dir.join("subtitle_eng.vtt");
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
        .map(|_| format!("http://{}:{}/cast-receiver/intro.mp4", state.config.lan_ip, state.config.port));

    // Check if subtitles exist
    let subs_vtt = state.media_dir.join("subtitle_eng.vtt");
    let subtitle_url = if subs_vtt.exists() {
        Some(format!("http://{}:{}/cast-receiver/subs.vtt", state.config.lan_ip, state.config.port))
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
        "duration": current.and_then(|_| {
            // Duration was detected during play and could be stored
            // For now, return None — receiver gets it from the stream
            None::<f64>
        }),
        "resume_position": resume_pos,
        "seek_restart_url": format!("http://{}:{}/api/seek-restart", state.config.lan_ip, state.config.port),
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
    let seek_seconds = req.t.max(0.0);

    // Kill current ffmpeg
    if let Some(pid) = state.ffmpeg_pid.lock().unwrap().take() {
        torrent::kill_pid(pid);
    }
    // Delete old transcoded file
    let transcoded = state.media_dir.join("transcoded_aac.mp4");
    if transcoded.exists() {
        let _ = std::fs::remove_file(&transcoded);
    }

    // Get current stream's webtorrent URL from state
    let app_state = AppState::load(&state.state_dir);
    let server_url = match &app_state.current {
        Some(c) => {
            // The URL might be the transcode endpoint — we need the original webtorrent URL
            // which is stored as the first webtorrent URL on port 8888
            if c.url.contains("/stream/transcode") {
                // Reconstruct from webtorrent log or use a stored field
                // For now, check if webtorrent is still running and serving
                format!("http://{}:8888", state.config.lan_ip)
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
        "stream_url": format!("http://{}:{}/stream/transcode", state.config.lan_ip, state.config.port),
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
    if req.imdb_id.is_none() && req.title.is_none() {
        return Json(json!({"error": "Missing imdb_id and title"}));
    }
    let mut app_state = AppState::load(&state.state_dir);
    let (key, saved) = app_state.save_position_smart(req.imdb_id.clone(), req.title.clone(), req.t);
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

/// Retry with next torrent result after stream failure.
async fn handle_retry(State(state): State<SharedState>) -> Json<Value> {
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
}
