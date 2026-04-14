use std::os::unix::fs::MetadataExt;
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

    let state_dir = Config::state_dir();
    let media_dir = config.media_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&media_dir)?;
    reconcile_webtorrent_workers_on_startup(&state_dir);

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

fn reconcile_webtorrent_workers_on_startup(state_dir: &PathBuf) {
    let mut app_state = AppState::load(state_dir);
    let active_pid = app_state
        .current
        .as_ref()
        .map(|current| current.pid)
        .filter(|pid| *pid > 0 && unsafe { torrent::kill_check(*pid) });

    if app_state.current.is_some() && active_pid.is_none() {
        tracing::warn!("Clearing stale current stream on startup: recorded WebTorrent PID is not running");
        app_state.current = None;
        let _ = app_state.save(state_dir);
    }

    let allowed: Vec<u32> = active_pid.into_iter().collect();
    let killed = torrent::kill_webtorrent_except(&allowed);
    if !killed.is_empty() {
        tracing::warn!("Terminated stale WebTorrent workers on startup: {:?}", killed);
    }

    let pid_path = state_dir.join("webtorrent.pid");
    if let Some(pid) = allowed.first() {
        let _ = torrent::save_pid(&pid_path, *pid);
    } else {
        let _ = std::fs::write(pid_path, "");
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

    let title = req.title.clone().unwrap_or_else(|| "Unknown".into());

    // --- SMART DISK HYGIENE ---
    // Proactively prune stale media.
    disk::prune_disk(&media_dir, &title);

    // Local Bypass System: Check if the movie already exists on disk
    let mut server_url = String::new();
    let mut pid: u32 = 0;
    let mut is_local = false;

    if let Some(title) = &req.title {
        // Search for the file in media_dir (YTS format: "Movie Title (Year) [Quality] ...")
        if let Ok(entries) = std::fs::read_dir(&media_dir) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    let folder_name = entry.file_name().to_string_lossy().to_string();
                    let matches_title = title_tokens_match(&folder_name, title);

                    if !matches_title {
                        tracing::debug!("Bypass Mismatch: '{}' vs '{}'", sanitize_title(title), sanitize_title(&folder_name));
                    }
                    let matches_year = if title.contains("2026") {
                        folder_name.contains("2026")
                    } else if title.contains("2025") {
                        folder_name.contains("2025")
                    } else {
                        true // No year in query, trust title match
                    };

                    // CRITICAL: Check Quality-Awareness to prevent downgrades (e.g., 4k vs 1080p)
                    let matches_quality = if let Some(q) = &req.quality {
                        let q_lower = q.to_lowercase();
                        if q_lower.contains("2160p") || q_lower.contains("4k") {
                            folder_name.contains("2160p") || folder_name.contains("4k") || folder_name.contains("2160")
                        } else if q_lower.contains("1080p") {
                            folder_name.contains("1080p") || folder_name.contains("1080")
                        } else {
                            true // Generic match
                        }
                    } else {
                        true // No quality specified
                    };

                    let expected_bytes = req.size.as_deref().and_then(parse_size_to_bytes).unwrap_or(0);
                    let has_done_marker = entry.path().join(".spela_done").exists();

                    if file_type.is_dir() && matches_title && matches_year && matches_quality {
                        // Found a matching directory, look for mp4/mkv inside
                        if let Ok(sub_entries) = std::fs::read_dir(entry.path()) {
                            for sub_entry in sub_entries.flatten() {
                                let path = sub_entry.path();
                                let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                                // Only match actual movie files, not transcode artifacts
                                if (ext == "mp4" || ext == "mkv") && !fname.starts_with("transcoded") {
                                    // Known expected size wins over any completion marker.
                                    if local_bypass_file_is_healthy(&path, has_done_marker, expected_bytes) {
                                        tracing::info!("Local Bypass: Found healthy file (done_marker: {}, physical_match: true): {:?}", has_done_marker, path);
                                        server_url = format!("file://{}", path.to_string_lossy());
                                        is_local = true;
                                        break;
                                    } else {
                                        tracing::info!("Local Bypass: Found file but failed health check (size: {}B, expected: {}B). Delegating to Torrent Engine.", path.metadata().map_or(0, |m| m.len()), expected_bytes);
                                    }
                                }
                            }
                        }
                    } else if file_type.is_file() && matches_title && matches_year && matches_quality {
                        // Top-level single-file release living directly in media_dir
                        // (e.g. webtorrent finishes a single-file torrent into
                        // ~/media/Some.Movie.1080p.x264.mkv with no parent folder).
                        // Without this branch, fully-downloaded top-level files
                        // would never be recognized for Local Bypass and every
                        // play call would re-fetch the torrent — the exact bug
                        // that left a 4.2 GB FLUX file invisible to Bypass on
                        // Apr 15, 2026.
                        let path = entry.path();
                        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                        if (ext == "mp4" || ext == "mkv") && !fname.starts_with("transcoded") {
                            // Trust the title/year/quality match for content identity
                            // and only sanity-check the file via top_level_file_is_healthy
                            // (≥100 MB, non-sparse). See the function's doc for why we
                            // don't enforce strict size matching against the search
                            // result's expected_bytes here.
                            if top_level_file_is_healthy(&path) {
                                tracing::info!(
                                    "Local Bypass: Found healthy top-level file (logical {}B, expected {}B, title-trust): {:?}",
                                    path.metadata().map_or(0, |m| m.len()),
                                    expected_bytes,
                                    path
                                );
                                server_url = format!("file://{}", path.to_string_lossy());
                                is_local = true;
                            } else {
                                tracing::info!(
                                    "Local Bypass: Top-level file failed sanity check (size: {}B, sparse-or-tiny). Delegating to Torrent Engine.",
                                    path.metadata().map_or(0, |m| m.len())
                                );
                            }
                        }
                    }
                }
                if is_local { break; }
            }
        }
    }

    // Stop existing stream (webtorrent and ffmpeg)
    let pid_path = state.state_dir.join("webtorrent.pid");
    torrent::stop_by_pid_file(&pid_path);
    if let Some(old_fb_pid) = state.ffmpeg_pid.lock().unwrap().take() {
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

    let target = req.target.as_deref().unwrap_or(&app_state.preferences.default_target).to_string();
    let cast_name = req.cast_name.clone()
        .or_else(|| app_state.preferences.chromecast_name.clone())
        .unwrap_or_else(|| state.config.default_device.clone());
    let no_subs = req.no_subs.unwrap_or(false);
    let sub_lang = req.subtitle_lang.clone().unwrap_or_else(|| "eng".into());

    // Start webtorrent if NOT local
    if !is_local {
        // Disk check: Only required if we are going to start a NEW download
        if let Ok(Some(err)) = disk::check_space(&media_dir) {
            return Json(json!({"error": err}));
        }

        let log_path = state.state_dir.join("webtorrent.log");
        let result = match torrent::start_webtorrent(
            &magnet, req.file_index, &media_dir, &state.config.stream_host, &log_path
        ).await {
            Ok(r) => r,
            Err(e) => return Json(json!({"error": e.to_string()})),
        };
        pid = result.0;
        server_url = result.1;
        let _ = torrent::save_pid(&state.state_dir.join("webtorrent.pid"), pid);

        // Self-healing: check download progress
        if !torrent::check_progress(&log_path, 12).await {
            tracing::warn!("Torrent has no download progress after 12s — dead seeds");
            torrent::kill_pid(pid);
            torrent::kill_all_webtorrent();
            disk::prune_disk(&media_dir, ""); // Clean up any dead attempt
            return Json(json!({"error": "Torrent has no active seeds (0% after 12s)"}));
        }
    }

    // Fetch subtitles FIRST (needed for burn-in during transcode)
    let mut has_subtitles = false;
    let mut subtitle_srt_path: Option<PathBuf> = None;
    if !no_subs {
        if let Some(imdb_id) = &req.imdb_id {
            let client = reqwest::Client::new();
            match subtitles::fetch_subtitles(&client, imdb_id, req.season, req.episode, &sub_lang, &state.media_dir).await {
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

    let (video_codec, audio_codec, source_duration) = transcode::detect_codecs(&server_url).await
        .unwrap_or((None, None, None));
    if let Some(dur) = source_duration {
        tracing::info!("Source duration: {:.0}s ({:.0} min)", dur, dur / 60.0);
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
                    final_url = format!("http://{}:{}/stream/transcode", state.config.stream_host, state.config.port);
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
                    let mut cast = state_clone.cast.lock().unwrap();
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
        duration,
        quality: req.quality.clone(),
        size: req.size.clone(),
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
                    // Grace period: let Chromecast play remaining buffer.
                    // 45 minutes account for high-speed HW encoding (12x+).
                    tracing::info!("Reaper: ffmpeg finished for '{}', waiting 45m grace period...", title_for_log);
                    tokio::time::sleep(tokio::time::Duration::from_secs(2700)).await;

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

    sleep(Duration::from_secs(STARTUP_GRACE_SECS)).await;

    let started_at = match started_at {
        Some(s) => s,
        None => {
            tracing::warn!("cast_health_monitor: no started_at recorded for '{}', exiting", title_for_log);
            return;
        }
    };

    let mut consecutive_failures: u32 = 0;
    tracing::info!(
        "cast_health_monitor: started for '{}' on '{}' (poll every {}s, fail after {} consecutive idle/error)",
        title_for_log, cast_name, POLL_INTERVAL_SECS, IDLE_FAILURE_THRESHOLD
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
            let mut cast = state_clone.cast.lock().unwrap();
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

                if is_dead {
                    consecutive_failures += 1;
                    tracing::warn!(
                        "cast_health_monitor: '{}' player_state={} ({}/{} consecutive idle polls before cleanup)",
                        title_for_log, info.player_state, consecutive_failures, IDLE_FAILURE_THRESHOLD
                    );
                } else {
                    if consecutive_failures > 0 {
                        tracing::info!(
                            "cast_health_monitor: '{}' recovered: player_state={} (was idle {} polls)",
                            title_for_log, info.player_state, consecutive_failures
                        );
                    }
                    consecutive_failures = 0;
                    tracing::debug!(
                        "cast_health_monitor: '{}' player_state={} time={:.0}/{:.0}",
                        title_for_log, info.player_state, info.current_time, info.duration
                    );
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
            tracing::error!(
                "cast_health_monitor: chromecast media session DEAD for '{}' ({} consecutive idle/error polls). Cleaning up workers.",
                title_for_log, consecutive_failures
            );
            do_cleanup(&state);
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
        duration: None,
        quality: Some(best.quality.clone()),
        size: Some(best.size.clone()),
    };

    handle_play(State(state.clone()), Json(play_req)).await
}

// --- Helpers ---

fn parse_size_to_bytes(size_str: &str) -> Option<u64> {
    let lower = size_str.to_lowercase();
    let parts: Vec<&str> = lower.split_whitespace().collect();
    if parts.len() < 2 { return None; }
    let val: f64 = parts[0].parse().ok()?;
    let unit = parts[1];
    let factor = match unit {
        "gb" | "gib" => 1024 * 1024 * 1024,
        "mb" | "mib" => 1024 * 1024,
        "kb" | "kib" => 1024,
        _ => 1,
    };
    Some((val * factor as f64) as u64)
}

fn local_bypass_file_is_healthy(path: &std::path::Path, has_done_marker: bool, expected_bytes: u64) -> bool {
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
    let mut media_dir = state.media_dir.clone();
    if media_dir.to_string_lossy().starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            media_dir = home.join(media_dir.strip_prefix("~/").unwrap());
        }
    }
    let media_dir = std::fs::canonicalize(&media_dir).unwrap_or(media_dir);
    let path = media_dir.join("transcoded_aac.mp4");
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
    if req.imdb_id.is_none() && req.title.is_none() {
        return Json(json!({"error": "Missing imdb_id and title"}));
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
