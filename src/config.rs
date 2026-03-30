use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default)]
    pub default_device: String,
    #[serde(default = "default_subtitles")]
    pub subtitles: String,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default)]
    pub tmdb_api_key: String,
    #[serde(default)]
    pub lan_ip: String,
    #[serde(default = "default_media_dir")]
    pub media_dir: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    /// Fallback IPs for Chromecast devices when mDNS discovery fails.
    /// Format: { "Device Name" = "192.168.x.x" }
    #[serde(default)]
    pub known_devices: HashMap<String, String>,
    /// Google Cast app ID for custom receiver. Leave empty to use Default Media Receiver.
    #[serde(default)]
    pub cast_app_id: String,
}

fn default_server() -> String { "localhost:7890".into() }
fn default_subtitles() -> String { "en".into() }
fn default_quality() -> String { "1080p".into() }
fn default_media_dir() -> String { "~/media".into() }
fn default_port() -> u16 { 7890 }
fn default_host() -> String { "0.0.0.0".into() }

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            default_device: String::new(),
            subtitles: default_subtitles(),
            quality: default_quality(),
            tmdb_api_key: String::new(),
            lan_ip: String::new(),
            media_dir: default_media_dir(),
            port: default_port(),
            host: default_host(),
            known_devices: HashMap::new(),
            cast_app_id: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();
        if config_path.exists() {
            let text = std::fs::read_to_string(&config_path)?;
            let mut config: Config = toml::from_str(&text)?;
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

    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(config_path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// True if this looks like a first run (no config file or no default device).
    pub fn needs_setup(&self) -> bool {
        self.default_device.is_empty()
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

    /// Auto-detect the machine's LAN IP by creating a UDP socket.
    /// Doesn't send any data — just checks which local address the OS would use.
    pub fn detect_lan_ip() -> Option<String> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("8.8.8.8:80").ok()?;
        Some(socket.local_addr().ok()?.ip().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_needs_setup() {
        let config = Config::default();
        assert!(config.needs_setup()); // default_device is empty
    }

    #[test]
    fn test_config_with_device_doesnt_need_setup() {
        let mut config = Config::default();
        config.default_device = "My TV".into();
        assert!(!config.needs_setup());
    }

    #[test]
    fn test_config_roundtrip_toml() {
        let mut config = Config::default();
        config.default_device = "Living Room".into();
        config.tmdb_api_key = "test-key".into();
        config.lan_ip = "10.0.0.1".into();
        config.known_devices.insert("TV".into(), "10.0.0.50".into());

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.default_device, "Living Room");
        assert_eq!(parsed.tmdb_api_key, "test-key");
        assert_eq!(parsed.lan_ip, "10.0.0.1");
        assert_eq!(parsed.known_devices.get("TV").unwrap(), "10.0.0.50");
    }

    #[test]
    fn test_config_from_minimal_toml() {
        // A config with just a TMDB key — everything else defaults
        let toml_str = r#"tmdb_api_key = "abc123""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.tmdb_api_key, "abc123");
        assert_eq!(config.port, 7890);
        assert!(config.default_device.is_empty());
        assert!(config.known_devices.is_empty());
    }

    #[test]
    fn test_config_known_devices_toml() {
        let toml_str = r#"
default_device = "Vardagsrum"

[known_devices]
"Vardagsrum" = "192.168.4.58"
"Bedroom TV" = "192.168.4.126"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.known_devices.len(), 2);
        assert_eq!(config.known_devices.get("Vardagsrum").unwrap(), "192.168.4.58");
    }

    #[test]
    fn test_detect_lan_ip() {
        // Should return a non-loopback IP on any machine with network
        let ip = Config::detect_lan_ip();
        if let Some(ip) = ip {
            assert!(!ip.is_empty());
            assert!(!ip.starts_with("127.")); // not loopback
        }
        // On CI without network, None is acceptable
    }

    #[test]
    fn test_media_dir_tilde_expansion() {
        let config = Config { media_dir: "~/media".into(), ..Config::default() };
        let expanded = config.media_dir();
        assert!(!expanded.to_string_lossy().contains('~'));
        assert!(expanded.to_string_lossy().contains("media"));
    }

    #[test]
    fn test_media_dir_absolute_path() {
        let config = Config { media_dir: "/tmp/spela-media".into(), ..Config::default() };
        assert_eq!(config.media_dir().to_string_lossy(), "/tmp/spela-media");
    }

    #[test]
    fn test_config_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.default_device.is_empty());
        assert_eq!(config.port, 7890);
        assert_eq!(config.server, "localhost:7890");
    }

    #[test]
    fn test_config_cast_app_id_default_empty() {
        let config = Config::default();
        assert!(config.cast_app_id.is_empty()); // uses Default Media Receiver when empty
    }
}
