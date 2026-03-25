use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default = "default_device")]
    pub default_device: String,
    #[serde(default = "default_subtitles")]
    pub subtitles: String,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default)]
    pub tmdb_api_key: String,
    #[serde(default = "default_lan_ip")]
    pub lan_ip: String,
    #[serde(default = "default_media_dir")]
    pub media_dir: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_server() -> String { "darwin.home:7890".into() }
fn default_device() -> String { "Fredriks TV".into() }
fn default_subtitles() -> String { "en".into() }
fn default_quality() -> String { "1080p".into() }
fn default_lan_ip() -> String { "192.168.4.1".into() }
fn default_media_dir() -> String { "~/media".into() }
fn default_port() -> u16 { 7890 }

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            default_device: default_device(),
            subtitles: default_subtitles(),
            quality: default_quality(),
            tmdb_api_key: String::new(),
            lan_ip: default_lan_ip(),
            media_dir: default_media_dir(),
            port: default_port(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();
        if config_path.exists() {
            let text = std::fs::read_to_string(&config_path)?;
            let mut config: Config = toml::from_str(&text)?;
            // Also check env for TMDB key
            if config.tmdb_api_key.is_empty() {
                if let Ok(key) = std::env::var("TMDB_API_KEY") {
                    config.tmdb_api_key = key;
                }
            }
            Ok(config)
        } else {
            let mut config = Config::default();
            if let Ok(key) = std::env::var("TMDB_API_KEY") {
                config.tmdb_api_key = key;
            }
            Ok(config)
        }
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("spela")
            .join("config.toml")
    }

    pub fn media_dir(&self) -> PathBuf {
        let expanded = self.media_dir.replace('~', &dirs::home_dir().unwrap_or_default().to_string_lossy());
        PathBuf::from(expanded)
    }

    pub fn state_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".spela")
    }
}
