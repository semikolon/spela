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
}
