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
    pub stream_host: String,
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
    /// Apr 28, 2026: Whether to send rich `Metadata::TvShow`/`Movie` in the
    /// LOAD message. Defaults to `false` because the **Default Media Receiver
    /// renders metadata-rich overlays as a permanent on-screen layer when the
    /// HLS playlist lacks `EXT-X-ENDLIST`** — which is always the case for
    /// spela's growing-during-transcode playlist (Cast Web Receiver only
    /// honors `EXT-X-ENDLIST` for VOD detection; `EXT-X-PLAYLIST-TYPE` and
    /// `MediaInfo.streamType` are explicitly ignored for this UI decision
    /// per Google's Cast team statement on the SDK forum).
    ///
    /// With metadata enabled: poster + title + season/episode block renders
    /// permanently along the bottom-left, plus the seek bar — ~25% screen
    /// occupied. Without metadata: just a thin progress line + elapsed-time
    /// counter at the very bottom edge — ~5% screen occupied.
    ///
    /// Flip to `true` once a Custom Cast Receiver is registered with the
    /// Cast SDK Developer Console ($5 one-time, blocks `cast_app_id` ≠ "")
    /// and deployed. Custom Receivers can programmatically hide the rich UI
    /// after a few seconds of inactivity, getting both the polish AND the
    /// auto-hide behavior. Until then the bare-bones UI is the lesser evil.
    ///
    /// Tracking issue: spela TODO.md § "Custom Receiver registration".
    /// Hard-won lesson context: spela CLAUDE.md § "DMR persistent overlay".
    #[serde(default)]
    pub rich_metadata_in_load: bool,
    /// Apr 28, 2026 [EXPERIMENTAL]: Append `#EXT-X-ENDLIST` to the playlist
    /// route response even though the playlist is still being written.
    /// Cast Web Receiver only honors `EXT-X-ENDLIST` for VOD detection (per
    /// Google's Cast team's own statement on the SDK forum), so adding it
    /// SHOULD trick the receiver into rendering VOD-style auto-hide
    /// controls. The risk: receiver may interpret current-segment-count as
    /// "total stream length" and stop fetching new segments after that
    /// point, truncating playback. If that happens, flip back to false.
    ///
    /// If this experiment works, we get auto-hide controls AND can safely
    /// re-enable `rich_metadata_in_load`. If it doesn't, fall back to the
    /// metadata-off compromise (small persistent overlay) until Custom
    /// Receiver lands.
    #[serde(default)]
    pub experimental_endlist_hack: bool,
    /// Apr 29, 2026: VOD-style manifest with predicted segment count + ENDLIST
    /// upfront, plus long-polled segment serving for not-yet-written segments.
    /// Receiver sees a complete VOD playlist, total duration matches reality
    /// (computed from `ss_offset` and source `duration`), controls auto-hide,
    /// no chase-the-end / current_time inflation.
    ///
    /// Two-part contract:
    ///   1. `handle_hls_playlist` parses ffmpeg's actual playlist, computes
    ///      avg EXTINF from emitted segments, predicts total = ceil(remaining
    ///      duration / avg) + 2-buffer, pads with placeholder segment names,
    ///      appends EXT-X-ENDLIST.
    ///   2. `handle_hls_segment` long-polls (up to 28s, < typical receiver
    ///      HTTP timeout) for not-yet-written segments. Receiver retries are
    ///      absorbed by the wait loop; it serves 200 OK as soon as ffmpeg
    ///      writes the segment, or 503 Retry-After if it never appears.
    ///
    /// Strictly better than `experimental_endlist_hack`: that one preserved
    /// only the segments-emitted-so-far list with appended ENDLIST, causing
    /// receiver to think total duration = current ffmpeg progress and chase
    /// the moving end marker (HWM-saving inflated, see Apr 28-29 incident).
    /// `vod_manifest_padded` declares the FULL duration upfront so the
    /// receiver's clock matches reality.
    ///
    /// Default off — requires field testing per stream type (long episodes,
    /// movies, edge-case sources). Flip on, watch one full episode, observe
    /// whether the receiver completes naturally or hits 503 on the trailing
    /// over-predicted segments.
    #[serde(default)]
    pub vod_manifest_padded: bool,
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
            stream_host: String::new(),
            media_dir: default_media_dir(),
            port: default_port(),
            host: default_host(),
            known_devices: HashMap::new(),
            cast_app_id: String::new(),
            rich_metadata_in_load: false,
            experimental_endlist_hack: false,
            vod_manifest_padded: false,
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
    #[allow(dead_code)]
    pub fn needs_setup(&self) -> bool {
        self.default_device.is_empty()
    }

    pub fn config_path() -> PathBuf {
        // Use ~/.config/spela/ on all platforms (including macOS) so the same
        // config file works whether spela runs as a Linux daemon or a macOS CLI.
        // `dirs::config_dir()` resolves to ~/Library/Application Support on macOS,
        // which silently hides an existing ~/.config/spela/config.toml and makes
        // the CLI fall back to the default `localhost:7890` server address.
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".config")
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

    /// Auto-detect a routable local IP fallback for `stream_host`.
    /// Doesn't send any data — just checks which local address the OS would use.
    pub fn detect_stream_host_fallback() -> Option<String> {
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
        config.stream_host = "media.local".into();
        config.known_devices.insert("TV".into(), "10.0.0.50".into());

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.default_device, "Living Room");
        assert_eq!(parsed.tmdb_api_key, "test-key");
        assert_eq!(parsed.stream_host, "media.local");
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
default_device = "Living Room TV"

[known_devices]
"Living Room TV" = "192.168.1.50"
"Bedroom TV" = "192.168.1.51"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.known_devices.len(), 2);
        assert_eq!(config.known_devices.get("Living Room TV").unwrap(), "192.168.1.50");
    }

    #[test]
    fn test_detect_stream_host_fallback() {
        // Should return a non-loopback IP on any machine with network
        let ip = Config::detect_stream_host_fallback();
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

    #[test]
    fn test_rich_metadata_in_load_defaults_off() {
        // Apr 28, 2026: Default OFF until Custom Receiver lands. With DMR
        // and metadata enabled, the rich-UI overlay never auto-hides because
        // spela's growing HLS playlist lacks EXT-X-ENDLIST → receiver thinks
        // it's a live stream → persistent overlay → ~25% screen occupied.
        // Default off keeps the bare progress bar (~5% screen occupied).
        // Flip to true when registering a Custom Receiver via Cast SDK
        // Developer Console.
        let config = Config::default();
        assert!(!config.rich_metadata_in_load,
            "Default must be OFF — DMR overlay-shrink trumps metadata polish");
    }

    #[test]
    fn test_rich_metadata_in_load_back_compat_on_old_toml() {
        // Pre-Apr-28 config files don't have rich_metadata_in_load. Adding
        // a new field must not break existing deployments — serde default.
        let toml_str = r#"
default_device = "Living Room TV"
tmdb_api_key = "abc"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.rich_metadata_in_load, "missing field must default to false");
    }

    #[test]
    fn test_rich_metadata_in_load_roundtrip() {
        let mut config = Config::default();
        config.rich_metadata_in_load = true;
        let s = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert!(parsed.rich_metadata_in_load);
    }

    #[test]
    fn test_config_path_uses_xdg_on_all_platforms() {
        // Regression: dirs::config_dir() returns ~/Library/Application Support on macOS,
        // which silently hid the real ~/.config/spela/config.toml and made the CLI fall
        // back to server="localhost:7890". The CLI and the Linux server must read the
        // same file regardless of which OS spela is running on.
        let path = Config::config_path();
        let s = path.to_string_lossy();
        assert!(s.ends_with("/.config/spela/config.toml"), "got {s}");
        assert!(
            !s.contains("Library/Application Support"),
            "config path must not resolve into macOS Application Support: {s}"
        );
    }
}
