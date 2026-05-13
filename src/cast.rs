use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use rust_cast::channels::media::{
    Image, Media, Metadata, MovieMediaMetadata, PlayerState, StreamType, TvShowMediaMetadata,
};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::{CastDevice, ChannelMessage};

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const CAST_PORT: u16 = 8009;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Apr 28, 2026: Inputs to `build_cast_metadata`, derived from the play
/// request + `last_search.json` context. Pure data — no network/IO. Default
/// is "all None" which produces `Metadata::None` (the legacy behavior),
/// preserving back-compat for any caller that doesn't have rich context.
///
/// Why a struct instead of 6 positional args to `cast_url`: the LOAD path
/// is going to keep growing (subtitle tracks, audio track index, custom
/// receiver app id …). A named struct lets future fields land additively
/// without forcing every caller to thread positional args through. Also
/// makes `build_cast_metadata` independently testable from `cast_url`'s
/// network plumbing.
#[derive(Debug, Clone, Default)]
pub struct CastMetadata {
    /// For TV: the episode title (rendered as subtitle in the rich UI).
    /// For movies: the movie title.
    pub title: Option<String>,
    /// For TV: the show name (e.g. "Hijack"). When `series_title` AND
    /// `season` AND `episode` are all set, `Metadata::TvShow` is emitted;
    /// otherwise we fall back to `Metadata::Movie` if `title` is set.
    pub series_title: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Full TMDB poster URL, already prefixed with the image base
    /// (`https://image.tmdb.org/t/p/w500{path}`).  Goes into
    /// `Vec<Image>` for receiver UI rendering.  Optional — controls
    /// auto-hiding is governed by stream type (live vs VOD HLS), not by
    /// the LOAD message metadata; see spela CLAUDE.md § "DMR overlay is
    /// stream-type-dependent".  Poster is purely the visual splash on top.
    pub poster_url: Option<String>,
    /// ISO-8601 release date for movies. Goes into `MovieMediaMetadata.subtitle`
    /// AND `release_date` so the receiver can render it under the title.
    pub release_date: Option<String>,
}

/// Build the rust_cast `Metadata` enum to put inside the LOAD message.
///
/// Apr 28, 2026 (Apr 29 corrected): Replaces the previous `metadata: None`
/// which made the Default Media Receiver fall back to its bare playback UI.
/// With proper metadata the receiver shows a poster + title splash on top
/// of the playback view.  See [Cast Web Receiver — Secondary
/// Image](https://developers.google.com/cast/docs/web_receiver/secondary_image)
/// for the receiver-side rendering rules.  Note: the persistent
/// progress-bar overlay is governed by stream type (live HLS vs VOD HLS),
/// NOT by this metadata — see spela CLAUDE.md § "DMR overlay is
/// stream-type-dependent" for the full case study.
///
/// Decision tree:
///
/// 1. Has `series_title` AND `season` AND `episode` → `TvShow`.
///    (`title` is treated as the episode_title; missing → just season+episode.)
/// 2. Else has non-empty `title` → `Movie`.
///    (`release_date` populated when present; subtitle defaults to the year.)
/// 3. Else → `None`. If we have nothing useful to show, sending bogus
///    placeholder metadata would be worse than letting DMR fall back —
///    at least the fallback path is what we had before this change.
///
/// Pure function: trivially testable, no network, no I/O, no global state.
pub fn build_cast_metadata(meta: &CastMetadata) -> Option<Metadata> {
    let images: Vec<Image> = match meta.poster_url.as_deref() {
        Some(url) if !url.trim().is_empty() => vec![Image::new(url.to_string())],
        _ => Vec::new(),
    };

    // Tier 1 — TV show: requires series_title + season + episode all present.
    // Apr 28: Season=0 / Episode=0 are legal (specials, pilots, teasers) so
    // we don't gate on >0; only require the fields to be Some.
    if let (Some(s), Some(e), Some(series)) = (
        meta.season,
        meta.episode,
        meta.series_title
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
    ) {
        let episode_title = meta
            .title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string());
        return Some(Metadata::TvShow(TvShowMediaMetadata {
            series_title: Some(series.to_string()),
            episode_title,
            season: Some(s),
            episode: Some(e),
            images,
            original_air_date: meta.release_date.clone(),
        }));
    }

    // Tier 2 — Movie: requires title.
    if let Some(title) = meta.title.as_deref().filter(|t| !t.trim().is_empty()) {
        // Subtitle: just the release year if we have a date, else None. The
        // receiver renders subtitle directly under the title — keeping it
        // short avoids overflow on TVs with smaller poster overlays.
        let subtitle = meta.release_date.as_deref().and_then(|d| {
            d.split('-')
                .next()
                .filter(|y| y.len() == 4)
                .map(|y| y.to_string())
        });
        return Some(Metadata::Movie(MovieMediaMetadata {
            title: Some(title.to_string()),
            subtitle,
            studio: None,
            images,
            release_date: meta.release_date.clone(),
        }));
    }

    None
}

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
    // Apr 25, 2026: surfaced for cast_health_monitor diagnostics so we can
    // distinguish IdleReason::Finished (natural EOF) from Interrupted/Error
    // (old-CrKey mid-stream death). Previously the monitor saw only
    // `player_state=IDLE` and had to guess. The underlying field in
    // rust_cast::channels::media::StatusEntry is Option<IdleReason>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_session_id: Option<i32>,
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
    /// Fallback IPs from config — used when mDNS and cache both miss.
    known_devices: HashMap<String, String>,
}

impl CastController {
    pub fn new(state_dir: &Path, known_devices: HashMap<String, String>) -> Self {
        let cache_path = state_dir.join("devices.json");
        let device_cache = Self::load_cache(&cache_path);
        Self {
            device_cache,
            cache_path,
            known_devices,
        }
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
        let receiver = mdns
            .browse(CAST_SERVICE)
            .map_err(|e| anyhow!("mDNS browse failed: {}", e))?;

        let mut devices = Vec::new();
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;

        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let name = info
                        .get_property_val_str("fn")
                        .unwrap_or("Unknown")
                        .to_string();
                    let model = info
                        .get_property_val_str("md")
                        .unwrap_or("Unknown")
                        .to_string();

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

    /// Resolve device name to IP, using cache → mDNS → config fallback.
    fn resolve_device(&mut self, name: &str) -> Result<(String, u16)> {
        if let Some(dev) = self.device_cache.get(name) {
            return Ok((dev.ip.clone(), dev.port));
        }
        if let Ok(devices) = self.discover() {
            if let Some(dev) = devices.iter().find(|d| d.name == name) {
                return Ok((dev.ip.clone(), dev.port));
            }
        }
        if let Some(ip) = self.known_devices.get(name) {
            return Ok((ip.clone(), CAST_PORT));
        }
        Err(anyhow!("Device '{}' not found. Run 'spela targets' to discover devices, then 'spela config default_device \"Name\"' to set default.", name))
    }

    /// Cast a URL to a named Chromecast device.
    ///
    /// `stream_type` is inferred from `content_type`:
    ///   - HLS playlists (`application/vnd.apple.mpegurl` or `application/x-mpegurl`)
    ///     → `StreamType::Buffered` because Default Media Receiver / CAF
    ///     treats HLS as a known-duration VOD source (even with EVENT-type
    ///     playlists, the receiver buffers segments and seeks within them).
    ///     Apr 15, 2026 live test discovered that `StreamType::Live` for HLS
    ///     made the receiver acknowledge the LOAD message but never fetch
    ///     the manifest URL — `player_state` stayed IDLE indefinitely.
    ///   - Everything else (video/mp4 + chunked-transfer fragmented MP4 from
    ///     the legacy /stream/transcode path) → `StreamType::Live` because
    ///     the file is growing and we don't want the receiver probing
    ///     Content-Length / asking for byte ranges.
    ///
    /// Duration passed for display purposes but seeking requires Custom
    /// Receiver — Default Media Receiver can't seek in fMP4 without
    /// byte-offset index. Jellyfin solves this with custom receiver + Shaka
    /// Player + server-side seek-restart.
    pub fn cast_url(
        &mut self,
        device_name: &str,
        url: &str,
        content_type: &str,
        duration: Option<f64>,
        _current_time: Option<f64>,
        metadata: &CastMetadata,
    ) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, session_id) = Self::get_or_launch_app(&device)?;

        let stream_type =
            if content_type.contains("mpegurl") || content_type.contains("application/dash+xml") {
                tracing::info!(
                    "cast_url: HLS/DASH content_type {:?} → StreamType::Buffered",
                    content_type
                );
                StreamType::Buffered
            } else {
                StreamType::Live
            };

        let cast_metadata = build_cast_metadata(metadata);
        if cast_metadata.is_none() {
            tracing::debug!(
                "cast_url: no rich metadata to send — Default Media Receiver \
                 will use minimal-overlay fallback UI"
            );
        }

        let media = Media {
            content_id: url.to_string(),
            content_type: content_type.to_string(),
            stream_type,
            duration: duration.map(|d| d as f32),
            metadata: cast_metadata,
        };

        // Note: rust_cast's Load message currently doesn't expose currentTime in its high-level API.
        // For now, we rely on the post-load seek in server.rs or the Custom Receiver's LOAD interceptor.
        let status = device
            .media
            .load(transport_id.as_str(), session_id.as_str(), &media)?;
        let media_session_id = status.entries.first().map(|e| e.media_session_id);

        // Wait briefly for playback to start, handling heartbeat pings
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match device.receive() {
                Ok(ChannelMessage::Heartbeat(_)) => {
                    let _ = device.heartbeat.pong();
                }
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
        device
            .media
            .pause(transport_id.as_str(), media_session_id)?;
        Ok(CastResult {
            status: "paused".into(),
            device: device_name.into(),
            url: None,
            media_session_id: Some(media_session_id),
        })
    }

    /// Resume playback on a device.
    pub fn resume(&mut self, device_name: &str) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.play(transport_id.as_str(), media_session_id)?;
        Ok(CastResult {
            status: "playing".into(),
            device: device_name.into(),
            url: None,
            media_session_id: Some(media_session_id),
        })
    }

    /// Stop playback on a device.
    #[allow(dead_code)]
    pub fn stop_cast(&mut self, device_name: &str) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.stop(transport_id.as_str(), media_session_id)?;
        Ok(CastResult {
            status: "stopped".into(),
            device: device_name.into(),
            url: None,
            media_session_id: None,
        })
    }

    /// Seek to a position (seconds) on a device.
    pub fn seek(&mut self, device_name: &str, seconds: f64) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        let (transport_id, media_session_id) = Self::get_active_media(&device)?;
        device.media.seek(
            transport_id.as_str(),
            media_session_id,
            Some(seconds as f32),
            None,
        )?;
        Ok(CastResult {
            status: "seeked".into(),
            device: device_name.into(),
            url: None,
            media_session_id: Some(media_session_id),
        })
    }

    /// Set volume (0-100) on a device.
    pub fn set_volume(&mut self, device_name: &str, level: u32) -> Result<CastResult> {
        let (ip, port) = self.resolve_device(device_name)?;
        let device = self.connect_with_retry(&ip, port)?;
        device
            .receiver
            .set_volume(rust_cast::channels::receiver::Volume {
                level: Some(level as f32 / 100.0),
                muted: Some(false),
            })?;
        Ok(CastResult {
            status: "volume_set".into(),
            device: device_name.into(),
            url: None,
            media_session_id: None,
        })
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
                        let title = entry
                            .media
                            .as_ref()
                            .and_then(|m| m.metadata.as_ref())
                            .map(|md| extract_metadata_title(md))
                            .unwrap_or_default();

                        return Ok(PlaybackInfo {
                            device: device_name.into(),
                            player_state: format!("{:?}", entry.player_state),
                            current_time: entry.current_time.unwrap_or(0.0) as f64,
                            duration: entry.media.as_ref().and_then(|m| m.duration).unwrap_or(0.0)
                                as f64,
                            volume,
                            muted,
                            title,
                            content_id: entry
                                .media
                                .as_ref()
                                .map(|m| m.content_id.clone())
                                .unwrap_or_default(),
                            idle_reason: entry.idle_reason.as_ref().map(|r| format!("{:?}", r)),
                            media_session_id: Some(entry.media_session_id),
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
            idle_reason: None,
            media_session_id: None,
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
        let app = device
            .receiver
            .launch_app(&CastDeviceApp::DefaultMediaReceiver)?;
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
        Metadata::TvShow(m) => m
            .episode_title
            .clone()
            .or_else(|| m.series_title.clone())
            .unwrap_or_default(),
        Metadata::MusicTrack(m) => m.title.clone().unwrap_or_default(),
        Metadata::Photo(m) => m.title.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Apr 28, 2026 (Apr 29 corrected): build_cast_metadata regression suite.
//
// The Default Media Receiver renders the LOAD message's `Media.metadata`
// field as a poster + title splash on top of the playback view:
//
//   - `metadata: Some(Metadata::TvShow|Movie)` with title + image →
//     poster background + episode/movie title overlay
//   - `metadata: None` → bare playback UI, no poster
//
// Pre-Apr-28 spela always sent `metadata: None`.  The Apr 28 fix routes
// show/episode/poster context from the play request through `CastMetadata`
// → `build_cast_metadata` → the LOAD message.
//
// **Apr 29 correction**: The original framing of these tests claimed
// metadata governed "auto-hide controls" vs "persistent overlay" — that
// was wrong.  The persistent progress-bar overlay is governed by stream
// type (live HLS vs VOD HLS), independent of metadata.  These tests
// nonetheless still pin the splash decision tree correctly; only the
// "what overlay state does each branch produce" framing was inverted.
// Full case study: spela CLAUDE.md § "DMR overlay is stream-type-dependent,
// not metadata-dependent".  Tests are creative + combinatorial per
// Fredrik's testing directive — each case is an invariant that, if
// broken, would re-introduce a specific real bug.
#[cfg(test)]
mod build_cast_metadata_tests {
    use super::*;

    fn meta() -> CastMetadata {
        CastMetadata::default()
    }

    // ---- Decision-tree happy path ----

    #[test]
    fn tv_show_with_all_fields_emits_tvshow_metadata() {
        let m = CastMetadata {
            title: Some("Marwan".into()),
            series_title: Some("Hijack".into()),
            season: Some(2),
            episode: Some(1),
            poster_url: Some("https://image.tmdb.org/t/p/w500/abc.jpg".into()),
            release_date: Some("2026-04-26".into()),
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert_eq!(t.series_title.as_deref(), Some("Hijack"));
                assert_eq!(t.episode_title.as_deref(), Some("Marwan"));
                assert_eq!(t.season, Some(2));
                assert_eq!(t.episode, Some(1));
                assert_eq!(t.images.len(), 1);
                assert_eq!(t.images[0].url, "https://image.tmdb.org/t/p/w500/abc.jpg");
                assert_eq!(t.original_air_date.as_deref(), Some("2026-04-26"));
            }
            other => panic!("expected TvShow, got {other:?}"),
        }
    }

    #[test]
    fn movie_with_title_emits_movie_metadata() {
        let m = CastMetadata {
            title: Some("Send Help".into()),
            series_title: None,
            season: None,
            episode: None,
            poster_url: Some("https://image.tmdb.org/t/p/w500/zzz.jpg".into()),
            release_date: Some("2026-01-15".into()),
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => {
                assert_eq!(mv.title.as_deref(), Some("Send Help"));
                assert_eq!(
                    mv.subtitle.as_deref(),
                    Some("2026"),
                    "Subtitle should be the year extracted from release_date"
                );
                assert_eq!(mv.images.len(), 1);
                assert_eq!(mv.release_date.as_deref(), Some("2026-01-15"));
            }
            other => panic!("expected Movie, got {other:?}"),
        }
    }

    #[test]
    fn nothing_useful_returns_none_so_dmr_falls_back_to_legacy_behavior() {
        // When neither title nor series_title is set, sending bogus
        // placeholder metadata would mislead the receiver. Returning None
        // preserves the pre-Apr-28 behavior for the truly-unknown case.
        assert!(build_cast_metadata(&meta()).is_none());
    }

    // ---- Partial / malformed input cases ----

    #[test]
    fn season_and_episode_without_series_title_falls_back_to_movie_or_none() {
        let m = CastMetadata {
            title: Some("Some Episode".into()),
            series_title: None,
            season: Some(2),
            episode: Some(1),
            ..meta()
        };
        // Without a series_title the TvShow branch can't fire — fall back
        // to Movie since title is set. Better to render rich Movie metadata
        // than the persistent-overlay fallback.
        assert!(matches!(build_cast_metadata(&m), Some(Metadata::Movie(_))));
    }

    #[test]
    fn series_title_without_season_or_episode_falls_back_to_movie() {
        // Edge: caller knows it's a TV show but doesn't know the episode
        // numbering yet (e.g. anthology series). Fall back to Movie shape
        // using the show name as the title.
        let m = CastMetadata {
            title: Some("Hijack".into()),
            series_title: Some("Hijack".into()),
            season: None,
            episode: None,
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => {
                assert_eq!(mv.title.as_deref(), Some("Hijack"));
            }
            other => panic!("expected Movie fallback, got {other:?}"),
        }
    }

    #[test]
    fn empty_string_series_title_treated_as_absent() {
        // An empty Some("") shouldn't activate the TvShow branch — the
        // receiver would render an empty header bar. Defense against
        // upstream bugs feeding us blank strings.
        let m = CastMetadata {
            title: Some("Episode".into()),
            series_title: Some("   ".into()), // whitespace-only
            season: Some(1),
            episode: Some(1),
            ..meta()
        };
        assert!(
            matches!(build_cast_metadata(&m), Some(Metadata::Movie(_))),
            "Whitespace-only series_title must not trigger TvShow path."
        );
    }

    #[test]
    fn empty_string_title_treated_as_absent() {
        let m = CastMetadata {
            title: Some("   ".into()),
            ..meta()
        };
        assert!(build_cast_metadata(&m).is_none());
    }

    #[test]
    fn season_zero_episode_zero_still_emits_tvshow() {
        // Season 0 = specials, Episode 0 = pilot/teaser — both legal
        // TVDB/TMDB conventions. Don't gate on >0.
        let m = CastMetadata {
            title: Some("Pilot".into()),
            series_title: Some("Show".into()),
            season: Some(0),
            episode: Some(0),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert_eq!(t.season, Some(0));
                assert_eq!(t.episode, Some(0));
            }
            other => panic!("expected TvShow for s0e0, got {other:?}"),
        }
    }

    #[test]
    fn tvshow_without_episode_title_still_works() {
        // Episode title unknown is fine — receiver renders just
        // "Show — S2E1" header.
        let m = CastMetadata {
            title: None,
            series_title: Some("Hijack".into()),
            season: Some(2),
            episode: Some(1),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert!(t.episode_title.is_none());
                assert_eq!(t.series_title.as_deref(), Some("Hijack"));
            }
            other => panic!("expected TvShow, got {other:?}"),
        }
    }

    // ---- Image / poster_url handling ----

    #[test]
    fn no_poster_url_yields_empty_images_vec() {
        let m = CastMetadata {
            title: Some("Test".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert_eq!(mv.images.len(), 0),
            other => panic!("expected Movie, got {other:?}"),
        }
    }

    #[test]
    fn empty_string_poster_url_yields_empty_images() {
        let m = CastMetadata {
            title: Some("Test".into()),
            poster_url: Some("".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert_eq!(mv.images.len(), 0),
            other => panic!("expected Movie, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_poster_url_yields_empty_images() {
        let m = CastMetadata {
            title: Some("Test".into()),
            poster_url: Some("   ".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert_eq!(mv.images.len(), 0),
            _ => unreachable!(),
        }
    }

    #[test]
    fn poster_url_passes_through_verbatim_no_double_encoding() {
        // URLs already contain percent-encoded chars from TMDB; we must
        // not re-encode them (would produce %25xx instead of %xx).
        let url = "https://image.tmdb.org/t/p/w500/path%20with%20spaces.jpg";
        let m = CastMetadata {
            title: Some("Test".into()),
            poster_url: Some(url.into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => {
                assert_eq!(mv.images[0].url, url);
            }
            _ => unreachable!(),
        }
    }

    // ---- Subtitle / release_date logic ----

    #[test]
    fn movie_subtitle_extracts_year_from_iso_date() {
        let m = CastMetadata {
            title: Some("Send Help".into()),
            release_date: Some("2026-01-15".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert_eq!(mv.subtitle.as_deref(), Some("2026")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn movie_subtitle_handles_year_only_release_date() {
        let m = CastMetadata {
            title: Some("Old Film".into()),
            release_date: Some("1985".into()),
            ..meta()
        };
        // "1985" splits to ["1985"], first element is 4-char numeric → year.
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert_eq!(mv.subtitle.as_deref(), Some("1985")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn movie_subtitle_skips_non_year_release_dates() {
        // Garbage release_date shouldn't produce a garbage subtitle.
        let m = CastMetadata {
            title: Some("X".into()),
            release_date: Some("not-a-date".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert!(mv.subtitle.is_none()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn movie_subtitle_none_when_no_release_date() {
        let m = CastMetadata {
            title: Some("X".into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => assert!(mv.subtitle.is_none()),
            _ => unreachable!(),
        }
    }

    // ---- Unicode / exotic input ----

    #[test]
    fn swedish_chars_preserved_in_title() {
        let m = CastMetadata {
            title: Some("Björk".into()),
            series_title: Some("Sång & Dans".into()),
            season: Some(1),
            episode: Some(1),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert_eq!(t.series_title.as_deref(), Some("Sång & Dans"));
                assert_eq!(t.episode_title.as_deref(), Some("Björk"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn long_episode_title_passes_through_untruncated() {
        // The receiver decides truncation/ellipsis itself based on the
        // overlay box. We must not pre-truncate or we lose info on TVs
        // with bigger overlays.
        let long_title = "a".repeat(500);
        let m = CastMetadata {
            title: Some(long_title.clone()),
            series_title: Some("S".into()),
            season: Some(1),
            episode: Some(1),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert_eq!(t.episode_title.as_deref(), Some(long_title.as_str()));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn title_with_quotes_and_apostrophes() {
        // "What's Past Is Prologue" — apostrophe + quotes should pass through
        let m = CastMetadata {
            title: Some(r#"What's "Past" Is Prologue"#.into()),
            ..meta()
        };
        match build_cast_metadata(&m) {
            Some(Metadata::Movie(mv)) => {
                assert_eq!(mv.title.as_deref(), Some(r#"What's "Past" Is Prologue"#));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn realistic_apr28_hijack_case_emits_tvshow() {
        // The actual case from the bug report screenshot: Hijack S2E1
        // playing on Fredriks TV with the persistent progress bar. With
        // metadata wired correctly this should produce TvShow metadata.
        let m = CastMetadata {
            title: Some("Hijack S02E01".into()),
            series_title: Some("Hijack".into()),
            season: Some(2),
            episode: Some(1),
            poster_url: Some(
                "https://image.tmdb.org/t/p/w500/qZQqEgXgGRpC8nJa9j5ej31Ynmm.jpg".into(),
            ),
            release_date: None,
        };
        match build_cast_metadata(&m) {
            Some(Metadata::TvShow(t)) => {
                assert_eq!(t.series_title.as_deref(), Some("Hijack"));
                assert_eq!(t.season, Some(2));
                assert_eq!(t.episode, Some(1));
                assert_eq!(t.images.len(), 1);
            }
            other => panic!("expected TvShow for the Apr 28 Hijack case, got {other:?}"),
        }
    }

    // ---- Default impl sanity ----

    #[test]
    fn default_castmetadata_produces_none() {
        assert!(build_cast_metadata(&CastMetadata::default()).is_none());
    }

    #[test]
    fn cast_metadata_clone_is_pure() {
        // CastMetadata is passed through tokio spawn_blocking — verify
        // Clone is honest (no shared state). If someone changes a field
        // to non-Clone-safe in the future this test will fail to compile.
        let m = CastMetadata {
            title: Some("x".into()),
            series_title: Some("y".into()),
            season: Some(1),
            episode: Some(1),
            poster_url: Some("https://x".into()),
            release_date: Some("2026".into()),
        };
        let c = m.clone();
        assert_eq!(m.title, c.title);
        assert_eq!(m.series_title, c.series_title);
    }
}
