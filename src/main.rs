mod audiobook;
mod books_app;
mod cast;
mod config;
mod disk;
mod following;
mod hls_cache;
mod library_origin;
mod podcast;
mod recommendations;
mod search;
mod server;
mod show_notes;
mod state;
mod subtitles;
mod torrent;
mod torrent_engine;
mod torrent_stream;
mod transcode;
mod uplink;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "spela",
    version,
    about = "AI-agent-ready media controller — search, stream, cast"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Server address (host:port) for CLI client mode
    #[arg(long, global = true)]
    server: Option<String>,

    /// Human-readable output (default: JSON)
    #[arg(long, global = true)]
    human: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the spela HTTP API server
    Server {
        /// Listen port
        #[arg(long, default_value_t = 7890)]
        port: u16,
        /// Listen host/IP address
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
    },
    /// v3.6.0 Local Library Streaming: run the library-host origin (a dumb
    /// matcher + Range file server). Run this on a machine that holds media
    /// the spela server can't see on its own filesystem; point the server's
    /// `remote_origins` config at `http://<this-host-lan-ip>:<port>`.
    /// See docs/LOCAL_LIBRARY_STREAMING_PLAN.md.
    ServeLibrary {
        /// Listen port (overrides config `library_serve_port`, default 7891)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Search for TV shows or movies
    Search {
        /// Search query (show name, optionally with season info)
        query: Vec<String>,
        /// Search for movies instead of TV shows
        #[arg(long)]
        movie: bool,
        /// Season number
        #[arg(long)]
        season: Option<u32>,
        /// Episode number
        #[arg(long)]
        episode: Option<u32>,
    },
    /// Play a search result (1-8) or magnet link. Example: spela play 1
    Play {
        /// Result number (1-8) from last search, OR a magnet link
        source: String,
        /// Stream to VLC instead of Chromecast
        #[arg(long)]
        vlc: bool,
        /// Phase 7 thin-client: no Chromecast — the server (the server, NVENC)
        /// builds the HLS as usual; THIS host plays it locally via mpv.
        /// Run on the renderer box. Heavy transcode stays on the
        /// server; this machine is a thin player. Blocks until mpv exits,
        /// then tells the server to stop. Display/GPU specifics live in
        /// the host's own ~/.config/mpv/mpv.conf (kiosk: vo=gpu/drm).
        #[arg(long)]
        local_renderer: bool,
        /// Chromecast device name
        #[arg(long)]
        cast: Option<String>,
        /// Title for state tracking
        #[arg(long)]
        title: Option<String>,
        /// File index override (auto-filled from search results)
        #[arg(long)]
        file_index: Option<u32>,
        /// Disable subtitles
        #[arg(long)]
        no_subs: bool,
        /// Disable intro clip
        #[arg(long)]
        no_intro: bool,
        /// Slower startup, fewer buffering stalls on Chromecast
        #[arg(long)]
        smooth: bool,
        /// Seek to position (e.g., 47:23 or 2100)
        #[arg(long)]
        seek: Option<String>,
    },
    /// Stop current stream
    Stop,
    /// Terminate spela's ffmpeg transcode workers + orphan pre-v3.3.0 webtorrent-cli processes without deleting media
    KillWorkers,
    /// Show playback status
    Status,
    /// Pause playback
    Pause,
    /// Resume playback
    Resume,
    /// Seek to absolute episode position in seconds. Omit to resume from saved
    /// HWM (the last position cast_health_monitor recorded for this stream).
    ///
    /// **v3.4.3 (May 14, 2026) semantics change**: the argument is the
    /// position you'd think of in the episode timeline, NOT the position
    /// within the transcoded HLS stream. So on a play that resumed at 14:30
    /// (`ss_offset=870`), `spela seek 870` jumps to 14:30 (stream-relative
    /// 0); `spela seek 0` errors with a hint pointing at `spela play --seek 0`
    /// because cast.seek can't reach before the current transcode's
    /// window. Pre-v3.4.3 `seek` took stream-relative position — that
    /// behavior is gone, and `spela seek 0` no longer "resets to start of
    /// stream". Use `spela seek` (no arg) for HWM-based resume instead.
    Seek {
        /// Absolute episode position in seconds. Omit to resume from saved HWM.
        position: Option<f64>,
    },
    /// Set volume (0-100)
    Volume {
        /// Volume level
        level: u32,
    },
    /// Play next episode
    Next,
    /// Play previous episode
    Prev,
    /// List available Chromecast devices
    Targets,
    /// Show watch history
    History,
    /// First-run setup: discover Chromecasts, pick default, save config
    Setup,
    /// Get or set preferences
    Config {
        /// Config key
        key: Option<String>,
        /// Config value
        value: Option<String>,
    },
    /// Recover a lost resume position
    Recover {
        /// IMDb ID or Title
        target: String,
        /// Position (e.g., 2843 or 47:23)
        position: String,
    },
    /// Clear a resume position
    Clear {
        /// IMDb ID or Title
        target: String,
    },
    /// Queue management — auto-fire next item on natural EOF.
    /// Examples:
    ///   spela queue add 1 --cast "Bedroom TV"   (queue search result #1)
    ///   spela queue list
    ///   spela queue clear
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Podcasts: find a show, list its episodes, play one (in VLC here, or --cast).
    /// Examples:
    ///   spela podcast search lex fridman
    ///   spela podcast episodes 1434243584
    ///   spela podcast play 1434243584                 (newest episode)
    ///   spela podcast play 1434243584 --episode 3 --cast "Living Room"
    Podcast {
        #[command(subcommand)]
        action: PodcastAction,
    },
    /// Audiobooks: search torrent indexes by title, play a book (in VLC here, or --cast).
    /// Examples:
    ///   spela audiobook search the body keeps the score
    ///   spela audiobook play <hash> --name "The Body Keeps the Score"
    Audiobook {
        #[command(subcommand)]
        action: AudiobookAction,
    },
}

#[derive(Subcommand)]
enum PodcastAction {
    /// Search Apple's podcast directory.
    Search {
        query: Vec<String>,
        /// Two-letter store code (default: the server's `podcast_country`).
        #[arg(long)]
        country: Option<String>,
    },
    /// A show's episodes, newest first.
    Episodes {
        /// Show id from `spela podcast search`.
        id: u64,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Play an episode. Default: the newest, in VLC on this machine.
    Play {
        id: u64,
        /// 1 = newest, 2 = the one before, and so on.
        #[arg(long, default_value_t = 1)]
        episode: usize,
        /// Chromecast device name instead of VLC.
        #[arg(long)]
        cast: Option<String>,
        /// Start at this many seconds in (VLC only).
        #[arg(long)]
        start: Option<u64>,
        /// Print the playlist URL instead of opening VLC.
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand)]
enum AudiobookAction {
    /// Search the Pirate Bay, Knaben and AudioBook Bay by title.
    Search { query: Vec<String> },
    /// Fetch a whole book, join its chapters into one .m4b and open it in the Books app
    /// (macOS). Books keeps its own copy; the .m4b is left in ~/Library/Caches/spela/books.
    Save {
        /// Info hash from `spela audiobook search`.
        hash: String,
        #[arg(long, default_value = "Audiobook")]
        name: String,
        /// Write the .m4b but do not open it in Books.
        #[arg(long)]
        no_import: bool,
        /// Where to write the .m4b (default ~/Library/Caches/spela/books).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Books saved to the Books app, with how far through each one is; --remove deletes
    /// the torrent and files of every finished one on the server.
    Finished {
        #[arg(long)]
        remove: bool,
    },
    /// Fetch a book's torrent and play it chapter by chapter.
    Play {
        /// Info hash from `spela audiobook search`.
        hash: String,
        #[arg(long, default_value = "Audiobook")]
        name: String,
        /// Chromecast device name instead of VLC (plays the first chapter).
        #[arg(long)]
        cast: Option<String>,
        /// Print the playlist URL instead of opening VLC.
        #[arg(long)]
        print: bool,
    },
}

/// Open a URL in VLC on the machine the CLI runs on.
fn open_in_local_vlc(url: &str) -> anyhow::Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .args(["-a", "VLC", url])
            .status()?
    } else {
        std::process::Command::new("vlc")
            .arg(url)
            .spawn()
            .map(|_| std::process::ExitStatus::default())?
    };
    if !status.success() {
        anyhow::bail!("could not open VLC");
    }
    Ok(())
}

#[derive(Subcommand)]
enum QueueAction {
    /// Add a search result (1-8 from last `spela search`) to the queue.
    Add {
        /// Result number from last `spela search`.
        result_id: usize,
        /// Override the default cast device (e.g. "Bedroom TV").
        #[arg(long)]
        cast: Option<String>,
        /// Slower startup, fewer buffering stalls on Chromecast
        #[arg(long)]
        smooth: bool,
    },
    /// List queue contents.
    List,
    /// Clear the queue.
    Clear,
}

/// Install tracing with a default filter that keeps spela's own log readable.
///
/// Measured 2026-08-31 over three days of the live journal: **14,074 of 16,390
/// lines — 86% — were a single librqbit message**, `"Outgoing ip … for peer is
/// in blocklist skipping"`, emitted at INFO from `torrent_state::live` once per
/// rejected peer. That is not a cosmetic annoyance: forensics on a real playback
/// failure the same night meant paging through thousands of blocklist lines to
/// find the handful of `spela::server` events that mattered, and any `-n`-bounded
/// look at the log showed nothing but noise.
///
/// What this costs: `torrent_state::live` also emits "torrent finished
/// downloading" (7 lines in those same three days) and per-chunk
/// "we already requested ChunkInfo …" spam. Losing the first to be rid of the
/// other two is the right trade — spela reports its own stream lifecycle — but
/// it IS a loss, so the module is silenced rather than the message, and
/// `RUST_LOG` still overrides everything for a debugging session:
///   `RUST_LOG=info,librqbit=debug spela server`
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,librqbit::torrent_state::live=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server { port, host } => {
            init_tracing();

            // Apr 30, 2026: rustls 0.23+ requires an explicit default
            // CryptoProvider before any TLS use. librqbit's tracker/UPnP HTTPS
            // path otherwise panics with "Could not automatically determine
            // the process-level CryptoProvider", poisoning whatever mutex the
            // panicking task was holding and cascading PoisonError into every
            // subsequent .lock().unwrap() (including the cast Mutex). `.ok()`
            // because if some other dep already installed a provider, the
            // second install returns Err and we don't care.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

            let mut config = config::Config::load().unwrap_or_default();
            config.port = port;
            config.host = host;
            if let Err(e) = server::run_server(config).await {
                eprintln!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::ServeLibrary { port } => {
            init_tracing();
            let config = config::Config::load().unwrap_or_default();
            if let Err(e) = library_origin::run(config, port).await {
                eprintln!("serve-library error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Setup => {
            run_setup().await;
            return;
        }
        Commands::KillWorkers => {
            let (webtorrent_pids, ffmpeg_pids) = torrent::kill_all_workers();
            let val = json!({
                "status": "workers_terminated",
                "webtorrent_pids": webtorrent_pids,
                "ffmpeg_pids": ffmpeg_pids,
                "media_deleted": false,
            });
            if cli.human {
                print_human(&val);
            } else {
                println!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
            }
            return;
        }
        _ => {
            let config = config::Config::load().unwrap_or_default();
            let server_addr = cli
                .server
                .or_else(|| std::env::var("SPELA_SERVER").ok())
                .unwrap_or(config.server.clone());

            let result = run_client_command(cli.command, &server_addr).await;
            match result {
                Ok(val) => {
                    if cli.human {
                        print_human(&val);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
                    }
                }
                Err(e) => {
                    let err = serde_json::json!({"error": e.to_string()});
                    if cli.human {
                        eprintln!("Error: {}", e);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&err).unwrap_or_default());
                    }
                    std::process::exit(1);
                }
            }
        }
    }
}

async fn run_setup() {
    use std::collections::HashMap;
    use std::io::{self, BufRead, Write};

    println!("🎬 spela setup\n");

    let mut config = config::Config::load().unwrap_or_default();

    // TMDB API key
    if config.tmdb_api_key.is_empty() {
        println!("You need a TMDB API key for search (free at https://themoviedb.org).");
        print!("TMDB API key (or press Enter to skip): ");
        io::stdout().flush().unwrap();
        let mut key = String::new();
        io::stdin().lock().read_line(&mut key).unwrap();
        let key = key.trim().to_string();
        if !key.is_empty() {
            config.tmdb_api_key = key;
        }
    }

    // Stream host — prefer a DNS hostname when available, fall back to a routable IP.
    if config.stream_host.is_empty() {
        if let Some(host) = config::Config::detect_stream_host_fallback() {
            println!("Detected stream host fallback: {}", host);
            config.stream_host = host;
        } else {
            print!("Could not auto-detect a stream host fallback. Enter hostname or IP manually: ");
            io::stdout().flush().unwrap();
            let mut host = String::new();
            io::stdin().lock().read_line(&mut host).unwrap();
            let host = host.trim().to_string();
            if !host.is_empty() {
                config.stream_host = host;
            }
        }
    }

    // Discover Chromecasts
    println!("\nScanning for Chromecast devices (5 seconds)...");
    let cast = cast::CastController::new(&config::Config::state_dir(), HashMap::new());
    // Discovery needs a mutable ref, so shadow
    let mut cast = cast;
    match cast.discover() {
        Ok(devices) if !devices.is_empty() => {
            println!("\nFound {} device(s):\n", devices.len());
            let unique: Vec<_> = {
                let mut seen = std::collections::HashSet::new();
                devices
                    .into_iter()
                    .filter(|d| seen.insert(d.name.clone()))
                    .collect()
            };
            for (i, dev) in unique.iter().enumerate() {
                println!("  {}. {} ({}) — {}", i + 1, dev.name, dev.ip, dev.model);
            }
            print!("\nDefault device [1-{}]: ", unique.len());
            io::stdout().flush().unwrap();
            let mut choice = String::new();
            io::stdin().lock().read_line(&mut choice).unwrap();
            if let Ok(idx) = choice.trim().parse::<usize>() {
                if idx >= 1 && idx <= unique.len() {
                    let dev = &unique[idx - 1];
                    config.default_device = dev.name.clone();
                    config
                        .known_devices
                        .insert(dev.name.clone(), dev.ip.clone());
                    println!("Default: {}", dev.name);
                }
            }

            // Save all discovered devices as known fallbacks
            for dev in &unique {
                config
                    .known_devices
                    .entry(dev.name.clone())
                    .or_insert(dev.ip.clone());
            }
        }
        _ => {
            println!("No devices found. Make sure Chromecasts are on and on the same network.");
            print!("Default device name (manual entry): ");
            io::stdout().flush().unwrap();
            let mut name = String::new();
            io::stdin().lock().read_line(&mut name).unwrap();
            let name = name.trim().to_string();
            if !name.is_empty() {
                config.default_device = name;
            }
        }
    }

    // Server address
    if config.server == "localhost:7890" {
        print!("\nServer address [localhost:7890]: ");
        io::stdout().flush().unwrap();
        let mut addr = String::new();
        io::stdin().lock().read_line(&mut addr).unwrap();
        let addr = addr.trim().to_string();
        if !addr.is_empty() {
            config.server = addr;
        }
    }

    match config.save() {
        Ok(_) => {
            println!(
                "\n✅ Config saved to {}",
                config::Config::config_path().display()
            );
            println!("\nNext steps:");
            println!("  1. Start the server:  spela server");
            println!("  2. Search:            spela search \"movie name\"");
            println!("  3. Play:              spela play 1");
        }
        Err(e) => eprintln!("Failed to save config: {}", e),
    }
}

async fn run_client_command(command: Commands, server: &str) -> anyhow::Result<Value> {
    let client = reqwest::Client::new();
    let base = format!("http://{}", server);

    match command {
        Commands::Search {
            query,
            movie,
            season,
            episode,
        } => {
            let q = query.join(" ");
            let mut url = format!("{}/search?q={}", base, urlencoded(&q));
            if movie {
                url.push_str("&movie=1");
            }
            if let Some(s) = season {
                url.push_str(&format!("&season={}", s));
            }
            if let Some(e) = episode {
                url.push_str(&format!("&episode={}", e));
            }
            Ok(client.get(&url).send().await?.json().await?)
        }
        Commands::Play {
            source,
            vlc,
            local_renderer,
            cast,
            title,
            file_index,
            no_subs,
            no_intro,
            smooth,
            seek,
        } => {
            // Smart source detection: number = result ID, magnet: = magnet link
            let is_result_id = source
                .parse::<usize>()
                .ok()
                .filter(|&n| (1..=20).contains(&n));
            let seek_to = if let Some(s) = seek {
                Some(parse_position_string(&s)?)
            } else {
                None
            };

            // Phase 7 thin-client: --local-renderer reuses the no-cast
            // "vlc" path (server builds HLS, no Chromecast) and plays it
            // here via mpv. Server (do_play) is UNCHANGED.
            let no_cast = vlc || local_renderer;
            let body = serde_json::json!({
                "result_id": is_result_id,
                "magnet": if is_result_id.is_none() { Some(&source) } else { None },
                "target": if no_cast { "vlc" } else { "chromecast" },
                "cast_name": cast,
                "title": title,
                "file_index": file_index,
                "no_subs": no_subs,
                "no_intro": no_intro,
                "smooth": smooth,
                "seek_to": seek_to,
            });
            let play_resp: Value = client
                .post(format!("{}/play", base))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            if !local_renderer {
                return Ok(play_resp);
            }

            // Thin-client render: wait for the server's HLS to be ready
            // (the server transcode cold-start), then play it locally with
            // mpv. Heavy transcode stays on the server's NVENC GPU; this
            // host is a dumb player. Blocks until mpv exits, then stops
            // the server-side stream. Display/GPU specifics belong in the
            // host's ~/.config/mpv/mpv.conf (kiosk: vo=gpu or vo=drm).
            let hls = format!("{}/hls/master.m3u8", base);
            eprintln!("local-renderer: server transcoding; waiting for HLS …");
            let mut ready = false;
            for _ in 0..60 {
                if let Ok(r) = client.get(&hls).send().await {
                    if r.status().is_success() {
                        ready = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            if !ready {
                let _ = client.post(format!("{}/stop", base)).send().await;
                return Err(anyhow::anyhow!(
                    "local-renderer: server HLS not ready after 60s ({})",
                    hls
                ));
            }
            eprintln!("local-renderer: launching mpv → {hls}");
            let status = tokio::process::Command::new("mpv")
                .arg("--fullscreen")
                .arg("--force-window=yes")
                .arg(&hls)
                .status()
                .await;
            // mpv exited (user quit / EOF) → release the server stream.
            let _ = client.post(format!("{}/stop", base)).send().await;
            match status {
                Ok(s) => Ok(serde_json::json!({
                    "local_renderer": true,
                    "mpv_exit_code": s.code(),
                    "play": play_resp
                })),
                Err(e) => Err(anyhow::anyhow!(
                    "local-renderer: failed to launch mpv ({e}). Is mpv installed (apt install mpv)?"
                )),
            }
        }
        Commands::Stop => Ok(client
            .post(format!("{}/stop", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Status => Ok(client
            .get(format!("{}/status", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Pause => Ok(client
            .post(format!("{}/pause", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Resume => Ok(client
            .post(format!("{}/resume", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Seek { position } => {
            // v3.4.3: `position` is absolute episode position. Omit to
            // resume from saved HWM. Server-side `handle_seek` converts to
            // stream-relative via `compute_cast_seek_target` (see its doc
            // for full semantics + error variants).
            let body = match position {
                Some(p) => serde_json::json!({"t": p}),
                None => serde_json::json!({}), // empty body → server looks up HWM
            };
            Ok(client
                .post(format!("{}/seek", base))
                .json(&body)
                .send()
                .await?
                .json()
                .await?)
        }
        Commands::Volume { level } => {
            let body = serde_json::json!({"level": level});
            Ok(client
                .post(format!("{}/volume", base))
                .json(&body)
                .send()
                .await?
                .json()
                .await?)
        }
        Commands::Next => Ok(client
            .post(format!("{}/next", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Prev => Ok(client
            .post(format!("{}/prev", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Targets => Ok(client
            .get(format!("{}/targets", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::History => Ok(client
            .get(format!("{}/history", base))
            .send()
            .await?
            .json()
            .await?),
        Commands::Config { key, value } => match (key, value) {
            (Some(k), Some(v)) => {
                let body = serde_json::json!({k: v});
                Ok(client
                    .post(format!("{}/config", base))
                    .json(&body)
                    .send()
                    .await?
                    .json()
                    .await?)
            }
            _ => Ok(client
                .get(format!("{}/config", base))
                .send()
                .await?
                .json()
                .await?),
        },
        Commands::Recover { target, position } => {
            let t = parse_position_string(&position)?;
            let is_imdb = target.starts_with("tt") && target.len() > 5;
            let body = if is_imdb {
                serde_json::json!({"imdb_id": target, "t": t})
            } else {
                serde_json::json!({"title": target, "t": t})
            };
            Ok(client
                .post(format!("{}/api/position", base))
                .json(&body)
                .send()
                .await?
                .json()
                .await?)
        }
        Commands::Clear { target } => {
            let is_imdb = target.starts_with("tt") && target.len() > 5;
            let body = if is_imdb {
                serde_json::json!({"imdb_id": target})
            } else {
                serde_json::json!({"title": target})
            };
            Ok(client
                .post(format!("{}/api/position/reset", base))
                .json(&body)
                .send()
                .await?
                .json()
                .await?)
        }
        Commands::Podcast { action } => match action {
            PodcastAction::Search { query, country } => {
                let mut url = format!("{}/podcast/search?q={}", base, urlencoded(&query.join(" ")));
                if let Some(c) = country {
                    url.push_str(&format!("&country={}", urlencoded(&c)));
                }
                Ok(client.get(&url).send().await?.json().await?)
            }
            PodcastAction::Episodes { id, limit, offset } => Ok(client
                .get(format!(
                    "{}/podcast/episodes?id={}&limit={}&offset={}",
                    base, id, limit, offset
                ))
                .send()
                .await?
                .json()
                .await?),
            PodcastAction::Play {
                id,
                episode,
                cast,
                start,
                print,
            } => {
                let n = episode.max(1) - 1;
                let page: Value = client
                    .get(format!(
                        "{}/podcast/episodes?id={}&limit=1&offset={}",
                        base, id, n
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(e) = page.get("error") {
                    anyhow::bail!("{}", e);
                }
                let ep = page["episodes"][0].clone();
                let Some(guid) = ep["guid"].as_str() else {
                    anyhow::bail!("no episode {} in that show", episode);
                };
                if let Some(device) = cast {
                    let r: Value = client
                        .post(format!("{}/podcast/cast", base))
                        .json(&serde_json::json!({"id": id, "ep": guid, "cast_name": device}))
                        .send()
                        .await?
                        .json()
                        .await?;
                    return Ok(
                        serde_json::json!({"show": page["title"], "episode": ep["title"], "cast": r}),
                    );
                }
                let mut url = format!("{}/podcast/vlc.m3u?id={}&ep={}", base, id, urlencoded(guid));
                if let Some(t) = start {
                    url.push_str(&format!("&t={}", t));
                }
                if !print {
                    open_in_local_vlc(&url)?;
                }
                Ok(
                    serde_json::json!({"show": page["title"], "episode": ep["title"],
                    "published": ep["published"], "playlist": url, "opened_in_vlc": !print}),
                )
            }
        },
        Commands::Audiobook { action } => match action {
            AudiobookAction::Search { query } => Ok(client
                .get(format!(
                    "{}/audiobook/search?q={}",
                    base,
                    urlencoded(&query.join(" "))
                ))
                .send()
                .await?
                .json()
                .await?),
            AudiobookAction::Finished { remove } => books_app::finished(&base, remove).await,
            AudiobookAction::Save {
                hash,
                name,
                no_import,
                dir,
            } => {
                let out_dir = dir.unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join("Library/Caches/spela/books")
                });
                books_app::save(&base, &hash, &name, !no_import, &out_dir).await
            }
            AudiobookAction::Play {
                hash,
                name,
                cast,
                print,
            } => {
                let started: Value = client
                    .post(format!("{}/audiobook/start", base))
                    .json(&serde_json::json!({"hash": hash, "name": name}))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(e) = started.get("error") {
                    anyhow::bail!("{}", e);
                }
                let id = started["id"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("no torrent id"))?;
                // The chapter list exists once the torrent's metadata has arrived.
                let mut files = Value::Null;
                for _ in 0..60 {
                    let f: Value = client
                        .get(format!("{}/audiobook/{}/files", base, id))
                        .send()
                        .await?
                        .json()
                        .await?;
                    if f["ready"].as_bool() == Some(true) {
                        files = f;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                let first = files["files"][0]["idx"].as_u64();
                let Some(first) = first else {
                    anyhow::bail!(
                        "the torrent's chapter list did not arrive within a minute (no peers?)"
                    );
                };
                if let Some(device) = cast {
                    let r: Value = client
                        .post(format!("{}/audiobook/{}/cast", base, id))
                        .json(&serde_json::json!({"file": first, "cast_name": device}))
                        .send()
                        .await?
                        .json()
                        .await?;
                    return Ok(
                        serde_json::json!({"book": name, "id": id, "chapters": files["files"].as_array().map(|a| a.len()), "cast": r}),
                    );
                }
                let url = format!("{}/audiobook/{}/open.m3u", base, id);
                if !print {
                    open_in_local_vlc(&url)?;
                }
                Ok(
                    serde_json::json!({"book": name, "id": id, "chapters": files["files"].as_array().map(|a| a.len()),
                    "playlist": url, "opened_in_vlc": !print}),
                )
            }
        },
        Commands::Queue { action } => match action {
            QueueAction::List => Ok(client
                .get(format!("{}/queue", base))
                .send()
                .await?
                .json()
                .await?),
            QueueAction::Clear => Ok(client
                .delete(format!("{}/queue", base))
                .send()
                .await?
                .json()
                .await?),
            QueueAction::Add {
                result_id,
                cast,
                smooth,
            } => {
                // Server resolves result_id against ITS last_search.json.
                // Required because the CLI runs on a different host than
                // the server in typical setups (Mac CLI → the server's spela).
                let body = serde_json::json!({
                    "result_id": result_id,
                    "cast_name": cast,
                    "smooth": smooth,
                });
                Ok(client
                    .post(format!("{}/queue", base))
                    .json(&body)
                    .send()
                    .await?
                    .json()
                    .await?)
            }
        },
        Commands::Server { .. }
        | Commands::Setup
        | Commands::KillWorkers
        | Commands::ServeLibrary { .. } => unreachable!(),
    }
}

fn print_human(val: &Value) {
    if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
        eprintln!("Error: {}", err);
        return;
    }

    // Search results
    if let Some(results) = val.get("results").and_then(|v| v.as_array()) {
        if let Some(show) = val.get("show") {
            let title = show.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let imdb = show.get("imdb_id").and_then(|v| v.as_str()).unwrap_or("");
            let status = show.get("status").and_then(|v| v.as_str()).unwrap_or("");
            println!("{} (IMDB: {}) — {}", title, imdb, status);
        }
        if let Some(ep) = val.get("searching") {
            let s = ep.get("season").and_then(|v| v.as_u64()).unwrap_or(0);
            let e = ep.get("episode").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("Searching: S{:02}E{:02}", s, e);
        }
        println!();
        for r in results {
            let id = r.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let quality = r.get("quality").and_then(|v| v.as_str()).unwrap_or("");
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let seeds = r.get("seeds").and_then(|v| v.as_u64()).unwrap_or(0);
            let size = r.get("size").and_then(|v| v.as_str()).unwrap_or("");
            let source = r.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let magnet = r.get("magnet").and_then(|v| v.as_str()).unwrap_or("");
            println!("  {}. [{}] {}", id, quality, title);
            println!("     {} seeds · {} · {}", seeds, size, source);
            println!("     {}...", &magnet[..magnet.len().min(80)]);
        }
        if let Some(next) = val.get("show").and_then(|s| s.get("next_episode")) {
            let s = next.get("season").and_then(|v| v.as_u64()).unwrap_or(0);
            let e = next.get("episode").and_then(|v| v.as_u64()).unwrap_or(0);
            let date = next.get("air_date").and_then(|v| v.as_str()).unwrap_or("?");
            println!("\nNext episode: S{:02}E{:02} on {}", s, e, date);
        }
        return;
    }

    // Status
    if let Some(status) = val.get("status").and_then(|v| v.as_str()) {
        println!("Status: {}", status);
        // Apr 15, 2026: surface auto-resume to the user so nobody is surprised
        // that a "fresh" play of an episode starts 37 minutes in. `resumed_from`
        // is only present (non-null) in the `play` response when do_play picked
        // up a saved HWM.
        if status == "streaming" {
            if let Some(resumed) = val.get("resumed_from").and_then(|v| v.as_f64()) {
                let total = resumed as u64;
                let h = total / 3600;
                let m = (total % 3600) / 60;
                let s = total % 60;
                if h > 0 {
                    println!(
                        "  ↩ Resuming at {}:{:02}:{:02} (from saved position)",
                        h, m, s
                    );
                } else {
                    println!("  ↩ Resuming at {}:{:02} (from saved position)", m, s);
                }
            }
            if let Some(t) = val.get("title").and_then(|v| v.as_str()) {
                if let Some(target) = val.get("target").and_then(|v| v.as_str()) {
                    println!("  Playing: {} → {}", t, target);
                }
            }
        }
        if status == "workers_terminated" {
            let webtorrent = val
                .get("webtorrent_pids")
                .and_then(|v| v.as_array())
                .map(|pids| pids.len())
                .unwrap_or(0);
            let ffmpeg = val
                .get("ffmpeg_pids")
                .and_then(|v| v.as_array())
                .map(|pids| pids.len())
                .unwrap_or(0);
            println!(
                "  Sent SIGTERM to {} WebTorrent worker(s) and {} ffmpeg worker(s).",
                webtorrent, ffmpeg
            );
            println!("  Media deleted: false");
            return;
        }
        if let Some(current) = val.get("current") {
            let title = current.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let target = current.get("target").and_then(|v| v.as_str()).unwrap_or("");
            println!("  Playing: {} → {}", title, target);
        }
    }

    // Targets
    if let Some(targets) = val.get("targets").and_then(|v| v.as_array()) {
        for t in targets {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let ip = t.get("ip").and_then(|v| v.as_str()).unwrap_or("");
            let model = t.get("model").and_then(|v| v.as_str()).unwrap_or("");
            println!("  {} ({}) — {}", name, ip, model);
        }
    }

    // History
    if let Some(history) = val.get("history").and_then(|v| v.as_array()) {
        for h in history.iter().take(10) {
            let date = h
                .get("watched_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .get(..16)
                .unwrap_or("");
            let title = h.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            println!("  {} {}", date, title);
        }
    }

    // Preferences
    if let Some(prefs) = val.get("preferences") {
        println!(
            "{}",
            serde_json::to_string_pretty(prefs).unwrap_or_default()
        );
    }

    // Generic streaming/cast result
    if let Some(pid) = val.get("pid").and_then(|v| v.as_u64()) {
        let title = val.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let target = val.get("target").and_then(|v| v.as_str()).unwrap_or("");
        println!("  {} → {} (PID: {})", title, target, pid);
    }
}

fn parse_position_string(s: &str) -> anyhow::Result<f64> {
    if let Ok(secs) = s.parse::<f64>() {
        return Ok(secs);
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        let m = parts[0].parse::<f64>()?;
        let s = parts[1].parse::<f64>()?;
        return Ok(m * 60.0 + s);
    } else if parts.len() == 3 {
        let h = parts[0].parse::<f64>()?;
        let m = parts[1].parse::<f64>()?;
        let s = parts[2].parse::<f64>()?;
        return Ok(h * 3600.0 + m * 60.0 + s);
    }
    anyhow::bail!("Invalid position format: use seconds (123) or MM:SS (47:23)")
}

fn urlencoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}
