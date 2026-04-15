use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::search::SearchResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub current: Option<CurrentStream>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub preferences: Preferences,
    /// Resume positions by IMDB ID (seconds). Used by Custom Cast Receiver.
    #[serde(default)]
    pub resume_positions: HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentStream {
    pub magnet: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    pub target: String,
    pub url: String,
    pub started_at: DateTime<Utc>,
    pub pid: u32,
    #[serde(default)]
    pub has_subtitles: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// The `-ss` seek offset passed to ffmpeg at transcode-start, in seconds
    /// of source-media timeline. Defaults to 0 for a normal play, becomes
    /// `N` when the user runs `spela play X --seek N` or when auto-resume
    /// picks up a saved baseline. Load-bearing for the smart resume feature:
    /// the Chromecast's `current_time` is relative to the transcoded stream
    /// (which starts at 0 no matter what `-ss` was), so to compute the
    /// absolute position in the source episode we need
    /// `absolute = chromecast_time + ss_offset`. Added Apr 15, 2026 to
    /// complete the "resume from where I stopped" feature for the Default
    /// Media Receiver path.
    #[serde(default)]
    pub ss_offset: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub title: String,
    pub watched_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default = "default_target")]
    pub default_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chromecast_name: Option<String>,
    #[serde(default = "default_quality")]
    pub preferred_quality: String,
}

fn default_target() -> String { "chromecast".into() }
fn default_quality() -> String { "1080p".into() }

impl Default for Preferences {
    fn default() -> Self {
        Self {
            default_target: default_target(),
            chromecast_name: None,
            preferred_quality: default_quality(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            current: None,
            history: Vec::new(),
            preferences: Preferences::default(),
            resume_positions: HashMap::new(),
        }
    }
}

impl AppState {
    pub fn load(state_dir: &PathBuf) -> Self {
        let path = state_dir.join("state.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, state_dir: &PathBuf) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("state.json");
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Save last search results to a separate file for play-by-id.
    pub fn save_last_search(state_dir: &Path, result: &SearchResult) {
        let path = state_dir.join("last_search.json");
        let _ = serde_json::to_string_pretty(result)
            .map(|s| std::fs::write(path, s));
    }

    /// Load last search results.
    pub fn load_last_search(state_dir: &Path) -> Option<SearchResult> {
        let path = state_dir.join("last_search.json");
        std::fs::read_to_string(path).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn stop_current(&mut self) {
        if let Some(current) = self.current.take() {
            self.history.insert(0, HistoryEntry {
                title: current.title,
                watched_at: Utc::now(),
                show: current.show,
                season: current.season,
                episode: current.episode,
                imdb_id: current.imdb_id,
                target: Some(current.target),
            });
            self.history.truncate(50);
        }
    }

    /// Update resume position for a movie/show using High-Water Mark logic.
    /// Returns (key, reset_or_saved).
    /// If duration is provided and t is near the end (>92% or within 300s), resets/clears the position.
    pub fn save_position_smart(&mut self, imdb_id: Option<String>, title: Option<String>, t: f64, duration: Option<f64>) -> (String, bool) {
        let key = if let Some(id) = imdb_id.as_ref().filter(|s| !s.is_empty()) {
            id.to_string()
        } else if let Some(t) = title.as_ref().filter(|s| !s.is_empty()) {
            slugify(&t)
        } else {
            return ("unknown".into(), false);
        };

        // --- Completion Logic ---
        // If we are within the last 5 minutes or past 92% of the film, clear the position.
        if let Some(dur) = duration {
            if dur > 0.0 && (t >= dur * 0.92 || t >= (dur - 300.0)) {
                tracing::info!("Playback completion detected for '{}' at {}s (of {}s) — clearing resume point", key, t, dur);
                self.reset_position(imdb_id, title);
                return (key, true);
            }
        }

        let current = self.resume_positions.get(&key).copied().unwrap_or(0.0);
        
        // --- High-Water Mark Logic ---
        // Only update if we've moved further than before.
        if t > current {
            self.resume_positions.insert(key.clone(), t);
            (key, true)
        } else {
            (key, false)
        }
    }

    /// Load resume position by IMDb ID or Title.
    pub fn get_position(&self, imdb_id: Option<String>, title: Option<String>) -> f64 {
        if let Some(id) = imdb_id.filter(|s| !s.is_empty()) {
            if let Some(pos) = self.resume_positions.get(&id) {
                return *pos;
            }
        }
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            let key = slugify(&t);
            return self.resume_positions.get(&key).copied().unwrap_or(0.0);
        }
        0.0
    }

    /// Force reset a resume position (bypasses High-Water Mark).
    pub fn reset_position(&mut self, imdb_id: Option<String>, title: Option<String>) -> String {
        let key = if let Some(id) = imdb_id.filter(|s| !s.is_empty()) {
            id
        } else if let Some(t) = title.filter(|s| !s.is_empty()) {
            slugify(&t)
        } else {
            return "unknown".into();
        };

        self.resume_positions.remove(&key);
        key
    }
}

/// Simple slugify for title-based keys.
fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_position_smart_completion_threshold() {
        let mut state = AppState::default();
        let imdb_id = Some("tt10548174".to_string());
        let title = Some("28 Years Later".to_string());
        
        // 1. Regular save (50 mins in)
        let (key, saved) = state.save_position_smart(imdb_id.clone(), title.clone(), 3000.0, Some(6907.0));
        assert!(saved);
        assert_eq!(state.resume_positions.get(&key), Some(&3000.0));

        // 2. Completion reset (1:49:00 out of 1:55:00)
        // 6540s / 6907s = 0.946 ( > 0.92 )
        let (key2, saved2) = state.save_position_smart(imdb_id.clone(), title.clone(), 6600.0, Some(6907.0));
        assert!(saved2);
        assert_eq!(state.resume_positions.get(&key2), None); // Should be cleared!
    }

    #[test]
    fn test_save_position_smart_completion_last_5_mins() {
        let mut state = AppState::default();
        let title = Some("Short Film".to_string());
        let duration = 600.0; // 10 mins

        // 8 mins in (80% but within 5 mins of end)
        let (key, saved) = state.save_position_smart(None, title.clone(), 480.0, Some(duration));
        assert!(saved);
        assert_eq!(state.resume_positions.get(&key), None); // Should be cleared!
    }

    /// CurrentStream.ss_offset must round-trip through JSON without loss.
    /// This is load-bearing for smart resume across server restarts: the
    /// cast_health_monitor snapshots ss_offset at startup, but if the server
    /// restarts mid-stream, the monitor re-reads state.json and must recover
    /// the same value to keep computing correct absolute positions. Apr 15,
    /// 2026 regression guard for the new field.
    #[test]
    fn test_current_stream_ss_offset_roundtrip() {
        let stream = CurrentStream {
            magnet: "magnet:?xt=urn:btih:abc".into(),
            title: "The Boys S05E01".into(),
            show: Some("The Boys".into()),
            season: Some(5),
            episode: Some(1),
            imdb_id: Some("tt1190634".into()),
            target: "chromecast:Fredriks TV".into(),
            url: "http://darwin.home:7890/hls/master.m3u8".into(),
            started_at: chrono::Utc::now(),
            pid: 0,
            has_subtitles: true,
            subtitle_lang: Some("eng".into()),
            duration: Some(3823.6),
            quality: Some("1080p".into()),
            size: Some("4.5 GB".into()),
            ss_offset: 1800.0,
        };
        let serialized = serde_json::to_string(&stream).expect("serialize");
        assert!(
            serialized.contains("\"ss_offset\":1800"),
            "ss_offset not in serialized form: {serialized}"
        );
        let restored: CurrentStream = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(restored.ss_offset, 1800.0);
    }

    /// Backwards compatibility: old state.json files from before Apr 15, 2026
    /// don't have `ss_offset`. They must still deserialize cleanly with
    /// the default value (0.0) instead of erroring out. Without the
    /// `#[serde(default)]` attribute on the field, spela's server would
    /// refuse to start on any host with a pre-upgrade state.json and the
    /// user would lose their watch history during the migration.
    #[test]
    fn test_current_stream_deserializes_without_ss_offset() {
        let legacy_json = r#"{
            "magnet": "magnet:?xt=urn:btih:abc",
            "title": "Old Movie",
            "target": "chromecast:TV",
            "url": "http://x/y",
            "started_at": "2026-04-14T12:00:00Z",
            "pid": 0
        }"#;
        let restored: CurrentStream = serde_json::from_str(legacy_json)
            .expect("legacy CurrentStream without ss_offset must deserialize");
        assert_eq!(restored.ss_offset, 0.0);
        assert_eq!(restored.title, "Old Movie");
    }

    /// Smoke test: the math for absolute position translation.
    /// chromecast's current_time is relative to the transcoded stream (which
    /// starts at 0 no matter what `-ss` was passed to ffmpeg). Absolute
    /// source-timeline position = chromecast_time + ss_offset.
    #[test]
    fn test_absolute_position_translation() {
        // Scenario: user ran `spela play 1 --seek 1800`, so ss_offset=1800.
        // Chromecast has been playing the transcoded stream for 845 seconds.
        // Absolute position in the original episode = 1800 + 845 = 2645s.
        let ss_offset = 1800.0_f64;
        let chromecast_time = 845.0_f64;
        let absolute = chromecast_time + ss_offset;
        assert_eq!(absolute, 2645.0);

        // The 92% completion threshold on a 3823.6s episode is 3517.7s.
        let duration = 3823.6_f64;
        assert!(absolute < duration * 0.92);
        // At 3517.8s absolute, we should be past the threshold.
        let near_end = 3517.8_f64;
        assert!(near_end >= duration * 0.92);
    }
}
