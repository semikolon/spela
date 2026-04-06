mod cast;
mod config;
mod disk;
mod search;
mod server;
mod state;
mod subtitles;
mod torrent;
mod transcode;

use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "spela", version, about = "AI-agent-ready media controller — search, stream, cast")]
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
    },
    /// Stop current stream
    Stop,
    /// Show playback status
    Status,
    /// Pause playback
    Pause,
    /// Resume playback
    Resume,
    /// Seek to position (seconds)
    Seek {
        /// Position in seconds
        seconds: f64,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server { port, host } => {
            tracing_subscriber::fmt::init();
            let mut config = config::Config::load().unwrap_or_default();
            config.port = port;
            config.host = host;
            if let Err(e) = server::run_server(config).await {
                eprintln!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Setup => {
            run_setup().await;
            return;
        }
        _ => {
            let config = config::Config::load().unwrap_or_default();
            let server_addr = cli.server
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
    use std::io::{self, Write, BufRead};
    use std::collections::HashMap;

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

    // LAN IP — auto-detect
    if config.lan_ip.is_empty() {
        if let Some(ip) = config::Config::detect_lan_ip() {
            println!("Detected LAN IP: {}", ip);
            config.lan_ip = ip;
        } else {
            print!("Could not auto-detect LAN IP. Enter manually: ");
            io::stdout().flush().unwrap();
            let mut ip = String::new();
            io::stdin().lock().read_line(&mut ip).unwrap();
            let ip = ip.trim().to_string();
            if !ip.is_empty() {
                config.lan_ip = ip;
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
                devices.into_iter().filter(|d| seen.insert(d.name.clone())).collect()
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
                    config.known_devices.insert(dev.name.clone(), dev.ip.clone());
                    println!("Default: {}", dev.name);
                }
            }

            // Save all discovered devices as known fallbacks
            for dev in &unique {
                config.known_devices.entry(dev.name.clone()).or_insert(dev.ip.clone());
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
            println!("\n✅ Config saved to {}", config::Config::config_path().display());
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
        Commands::Search { query, movie, season, episode } => {
            let q = query.join(" ");
            let mut url = format!("{}/search?q={}", base, urlencoded(&q));
            if movie { url.push_str("&movie=1"); }
            if let Some(s) = season { url.push_str(&format!("&season={}", s)); }
            if let Some(e) = episode { url.push_str(&format!("&episode={}", e)); }
            Ok(client.get(&url).send().await?.json().await?)
        }
        Commands::Play { source, vlc, cast, title, file_index, no_subs, no_intro } => {
            // Smart source detection: number = result ID, magnet: = magnet link
            let is_result_id = source.parse::<usize>().ok().filter(|&n| n >= 1 && n <= 20);
            let body = serde_json::json!({
                "result_id": is_result_id,
                "magnet": if is_result_id.is_none() { Some(&source) } else { None },
                "target": if vlc { "vlc" } else { "chromecast" },
                "cast_name": cast,
                "title": title,
                "file_index": file_index,
                "no_subs": no_subs,
                "no_intro": no_intro,
            });
            Ok(client.post(format!("{}/play", base)).json(&body).send().await?.json().await?)
        }
        Commands::Stop => Ok(client.post(format!("{}/stop", base)).send().await?.json().await?),
        Commands::Status => Ok(client.get(format!("{}/status", base)).send().await?.json().await?),
        Commands::Pause => Ok(client.post(format!("{}/pause", base)).send().await?.json().await?),
        Commands::Resume => Ok(client.post(format!("{}/resume", base)).send().await?.json().await?),
        Commands::Seek { seconds } => {
            Ok(client.get(format!("{}/seek?t={}", base, seconds)).send().await?.json().await?)
        }
        Commands::Volume { level } => {
            let body = serde_json::json!({"level": level});
            Ok(client.post(format!("{}/volume", base)).json(&body).send().await?.json().await?)
        }
        Commands::Next => Ok(client.post(format!("{}/next", base)).send().await?.json().await?),
        Commands::Prev => Ok(client.post(format!("{}/prev", base)).send().await?.json().await?),
        Commands::Targets => Ok(client.get(format!("{}/targets", base)).send().await?.json().await?),
        Commands::History => Ok(client.get(format!("{}/history", base)).send().await?.json().await?),
        Commands::Config { key, value } => {
            match (key, value) {
                (Some(k), Some(v)) => {
                    let body = serde_json::json!({k: v});
                    Ok(client.post(format!("{}/config", base)).json(&body).send().await?.json().await?)
                }
                _ => Ok(client.get(format!("{}/config", base)).send().await?.json().await?),
            }
        }
        Commands::Recover { target, position } => {
            let t = parse_position_string(&position)?;
            let is_imdb = target.starts_with("tt") && target.len() > 5;
            let body = if is_imdb {
                serde_json::json!({"imdb_id": target, "t": t})
            } else {
                serde_json::json!({"title": target, "t": t})
            };
            Ok(client.post(format!("{}/api/position", base)).json(&body).send().await?.json().await?)
        }
        Commands::Clear { target } => {
            let is_imdb = target.starts_with("tt") && target.len() > 5;
            let body = if is_imdb {
                serde_json::json!({"imdb_id": target})
            } else {
                serde_json::json!({"title": target})
            };
            Ok(client.post(format!("{}/api/position/reset", base)).json(&body).send().await?.json().await?)
        }
        Commands::Server { .. } | Commands::Setup => unreachable!(),
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
            let date = h.get("watched_at").and_then(|v| v.as_str()).unwrap_or("").get(..16).unwrap_or("");
            let title = h.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            println!("  {} {}", date, title);
        }
    }

    // Preferences
    if let Some(prefs) = val.get("preferences") {
        println!("{}", serde_json::to_string_pretty(prefs).unwrap_or_default());
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
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
            String::from(b as char)
        }
        _ => format!("%{:02X}", b),
    }).collect()
}
