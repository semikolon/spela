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
}
