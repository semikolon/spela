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
    /// Get or set preferences
    Config {
        /// Config key
        key: Option<String>,
        /// Config value
        value: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server { port } => {
            tracing_subscriber::fmt::init();
            let mut config = config::Config::load().unwrap_or_default();
            config.port = port;
            if let Err(e) = server::run_server(config).await {
                eprintln!("Server error: {}", e);
                std::process::exit(1);
            }
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
        Commands::Server { .. } => unreachable!(),
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

fn urlencoded(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
            String::from(b as char)
        }
        _ => format!("%{:02X}", b),
    }).collect()
}
