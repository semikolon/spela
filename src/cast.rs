use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use rust_cast::channels::media::{Media, Metadata, PlayerState, StreamType};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::{CastDevice, ChannelMessage};

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const CAST_PORT: u16 = 8009;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Known device IPs — fallback when mDNS fails.
const KNOWN_DEVICES: &[(&str, &str)] = &[
    ("Fredriks TV", "192.168.4.126"),
    ("Vardagsrum", "192.168.4.58"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackInfo {
    pub device: String,
    pub player_state: String,
    pub current_time: f64,
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
    pub title: String,
    pub content_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CastResult {
    pub status: String,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_session_id: Option<i32>,
}

pub struct CastController {
    device_cache: HashMap<String, DeviceInfo>,
    cache_path: PathBuf,
}

impl CastController {
    pub fn new(state_dir: &Path) -> Self {
        let cache_path = state_dir.join("devices.json");
        let device_cache = Self::load_cache(&cache_path);
        Self { device_cache, cache_path }
    }

    fn load_cache(path: &Path) -> HashMap<String, DeviceInfo> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_cache(&self) {
        let _ = serde_json::to_string_pretty(&self.device_cache)
            .map(|s| std::fs::write(&self.cache_path, s));
    }

    /// Discover Chromecast devices via mDNS.
    pub fn discover(&mut self) -> Result<Vec<DeviceInfo>> {
        let mdns = ServiceDaemon::new().map_err(|e| anyhow!("mDNS init failed: {}", e))?;
        let receiver = mdns.browse(CAST_SERVICE).map_err(|e| anyhow!("mDNS browse failed: {}", e))?;

        let mut devices = Vec::new();
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;

        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let name = info.get_property_val_str("fn")
                        .unwrap_or("Unknown").to_string();
                    let model = info.get_property_val_str("md")
                        .unwrap_or("Unknown").to_string();

                    if let Some(addr) = info.get_addresses().iter().next() {
                        let device = DeviceInfo {
                            name: name.clone(),
                            ip: addr.to_string(),
                            port: info.get_port(),
                            model,
                        };
                        self.device_cache.insert(name, device.clone());
                        devices.push(device);
                    }
                }
                _ => {}
            }
        }

        let _ = mdns.shutdown();
        self.save_cache();
        Ok(devices)
    }

    /// Resolve device name to IP, using cache then mDNS then hardcoded fallback.
    fn resolve_device(&mut self, name: &str) -> Result<(String, u16)> {
        if let Some(dev) = self.device_cache.get(name) {
            return Ok((dev.ip.clone(), dev.port));
        }
        if let Ok(devices) = self.discover() {
            if let Some(dev) = devices.iter().find(|d| d.name == name) {
                return Ok((dev.ip.clone(), dev.port));
            }
        }
        for (known_name, known_ip) in KNOWN_DEVICES {
            if *known_name == name {
                return Ok((known_ip.to_string(), CAST_PORT));
            }
        }
        Err(anyhow!("Device '{}' not found. Run 'spela targets' to discover devices.", name))
    }

    /// Cast a URL to a named Chromecast device.
    /// Uses StreamType::Live for transcoded streams (chunked, growing file).
    /// Duration passed for display purposes but seeking requires Custom Receiver —
    /// Default Media Receiver can't seek in fMP4 without byte-offset index.
    /// Jellyfin solves this with custom receiver + Shaka Player + server-side seek-restart.
    pub fn cast_url(&mut self, device_name: &str, url: &str, content_type: &str, duration: Option<f64>) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, session_id) = Self::get_or_launch_app(&device)?;

        let media = Media {
            content_id: url.to_string(),
            content_type: content_type.to_string(),
            stream_type: StreamType::Live,
            duration: duration.map(|d| d as f32),
            metadata: None,
        };

        let status = device.media.load(
            transport_id.as_str(),
            session_id.as_str(),
            &media,
        )?;
        let media_session_id = status.entries.first().map(|e| e.media_session_id);

        // Wait briefly for playback to start, handling heartbeat pings
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match device.receive() {
                Ok(ChannelMessage::Heartbeat(_)) => { let _ = device.heartbeat.pong(); }
                Ok(ChannelMessage::Media(rust_cast::channels::media::MediaResponse::Status(s))) => {
                    if let Some(entry) = s.entries.first() {
                        match entry.player_state {
                            PlayerState::Playing | PlayerState::Buffering => {
                                return Ok(CastResult {
                                    status: "casting".into(),
                                    device: device_name.into(),
                                    url: Some(url.to_string()),
                                    media_session_id: Some(entry.media_session_id),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(CastResult {
            status: "casting".into(),
            device: device_name.into(),
            url: Some(url.to_string()),
            media_session_id,
        })
    }

    /// Pause playback on a device.
    pub fn pause(&mut self, device_name: &str) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.pause(transport_id.as_str(), media_session_id)?;
        Ok(CastResult { status: "paused".into(), device: device_name.into(), url: None, media_session_id: Some(media_session_id) })
    }

    /// Resume playback on a device.
    pub fn resume(&mut self, device_name: &str) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.play(transport_id.as_str(), media_session_id)?;
        Ok(CastResult { status: "playing".into(), device: device_name.into(), url: None, media_session_id: Some(media_session_id) })
    }

    /// Stop playback on a device.
    pub fn stop_cast(&mut self, device_name: &str) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.stop(transport_id.as_str(), media_session_id)?;
        Ok(CastResult { status: "stopped".into(), device: device_name.into(), url: None, media_session_id: None })
    }

    /// Seek to a position (seconds) on a device.
    pub fn seek(&mut self, device_name: &str, seconds: f64) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.seek(transport_id.as_str(), media_session_id, Some(seconds as f32), None)?;
        Ok(CastResult { status: "seeked".into(), device: device_name.into(), url: None, media_session_id: Some(media_session_id) })
    }

    /// Set volume (0-100) on a device.
    pub fn set_volume(&mut self, device_name: &str, level: u32) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        device.receiver.set_volume(rust_cast::channels::receiver::Volume {
            level: Some(level as f32 / 100.0),
            muted: Some(false),
        })?;
        Ok(CastResult { status: "volume_set".into(), device: device_name.into(), url: None, media_session_id: None })
    }

    /// Get playback info from a device.
    pub fn get_info(&mut self, device_name: &str) -> Result<PlaybackInfo> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;

        let receiver_status = device.receiver.get_status()?;
        let volume = receiver_status.volume.level.unwrap_or(0.0) as f64;
        let muted = receiver_status.volume.muted.unwrap_or(false);

        for app in &receiver_status.applications {
            if app.app_id == "CC1AD845" {
                device.connection.connect(app.transport_id.as_str())?;
                if let Ok(media_status) = device.media.get_status(app.transport_id.as_str(), None) {
                    if let Some(entry) = media_status.entries.first() {
                        let title = entry.media.as_ref()
                            .and_then(|m| m.metadata.as_ref())
                            .map(|md| extract_metadata_title(md))
                            .unwrap_or_default();

                        return Ok(PlaybackInfo {
                            device: device_name.into(),
                            player_state: format!("{:?}", entry.player_state),
                            current_time: entry.current_time.unwrap_or(0.0) as f64,
                            duration: entry.media.as_ref().and_then(|m| m.duration).unwrap_or(0.0) as f64,
                            volume,
                            muted,
                            title,
                            content_id: entry.media.as_ref()
                                .map(|m| m.content_id.clone())
                                .unwrap_or_default(),
                        });
                    }
                }
            }
        }

        Ok(PlaybackInfo {
            device: device_name.into(),
            player_state: "IDLE".into(),
            current_time: 0.0,
            duration: 0.0,
            volume,
            muted,
            title: String::new(),
            content_id: String::new(),
        })
    }

    /// Connect to a device with retry logic. Uses owned String for host to avoid lifetime issues.
    fn connect_with_retry(&self, ip: &str, port: u16) -> Result<CastDevice<'static>> {
        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                std::thread::sleep(RETRY_DELAY);
            }
            // Pass owned String so CastDevice gets Cow::Owned — no borrow lifetime issues
            match CastDevice::connect_without_host_verification(ip.to_string(), port) {
                Ok(device) => {
                    if let Err(e) = device.connection.connect("receiver-0") {
                        last_err = Some(anyhow!("Connection setup failed: {}", e));
                        continue;
                    }
                    if let Err(e) = device.heartbeat.ping() {
                        last_err = Some(anyhow!("Heartbeat failed: {}", e));
                        continue;
                    }
                    return Ok(device);
                }
                Err(e) => {
                    last_err = Some(anyhow!("Connect attempt {}: {}", attempt + 1, e));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("Failed to connect after {} retries", MAX_RETRIES)))
    }

    /// Get or launch the Default Media Receiver app on the device.
    fn get_or_launch_app(device: &CastDevice) -> Result<(String, String)> {
        let status = device.receiver.get_status()?;
        for app in &status.applications {
            if app.app_id == "CC1AD845" {
                device.connection.connect(app.transport_id.as_str())?;
                return Ok((app.transport_id.clone(), app.session_id.clone()));
            }
        }
        let app = device.receiver.launch_app(&CastDeviceApp::DefaultMediaReceiver)?;
        device.connection.connect(app.transport_id.as_str())?;
        Ok((app.transport_id, app.session_id))
    }

    /// Find active media session on a device.
    fn get_active_media(device: &CastDevice) -> Result<(String, i32)> {
        let status = device.receiver.get_status()?;
        for app in &status.applications {
            if app.app_id == "CC1AD845" {
                device.connection.connect(app.transport_id.as_str())?;
                if let Ok(media_status) = device.media.get_status(app.transport_id.as_str(), None) {
                    if let Some(entry) = media_status.entries.first() {
                        return Ok((app.transport_id.clone(), entry.media_session_id));
                    }
                }
                return Err(anyhow!("No active media session"));
            }
        }
        Err(anyhow!("No media app running on device"))
    }
}

fn extract_metadata_title(metadata: &Metadata) -> String {
    match metadata {
        Metadata::Generic(m) => m.title.clone().unwrap_or_default(),
        Metadata::Movie(m) => m.title.clone().unwrap_or_default(),
        Metadata::TvShow(m) => m.episode_title.clone()
            .or_else(|| m.series_title.clone())
            .unwrap_or_default(),
        Metadata::MusicTrack(m) => m.title.clone().unwrap_or_default(),
        Metadata::Photo(m) => m.title.clone().unwrap_or_default(),
    }
}
