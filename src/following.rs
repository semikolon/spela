//! Followed-series tracking (roadmap slice 2b, "Up Next"). TVTime replacement.
//!
//! A user-local `~/.config/spela/following.json` lists ongoing series Fredrik
//! actively follows, each with a `watched_through` baseline (SxxExx). spela joins
//! it with TMDB air-dates (`SearchEngine::tv_status`) to compute, per show, the
//! next-unwatched episode + how many aired episodes are new since he last watched.
//! `watched_through` is advanced automatically when he finishes an episode via
//! spela, and manually via `POST /following/mark` (for views on a phone / a
//! friend's Chromecast / another app that spela can't see).
//!
//! Deterministic — no LLM. Personal data → never committed (public repo); lives
//! only on the spela host next to `config.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowedShow {
    pub title: String,
    pub tmdb_id: u64,
    /// Latest episode watched, "S03E04". None = nothing watched yet (all aired
    /// episodes count as new).
    #[serde(default)]
    pub watched_through: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Following {
    #[serde(default)]
    pub shows: Vec<FollowedShow>,
}

/// `~/.config/spela/following.json` — next to config.toml (same hardcoded-XDG
/// resolution as `Config::config_path`, which avoids macOS's Application Support).
pub fn following_path() -> PathBuf {
    crate::config::Config::config_path()
        .parent()
        .map(|p| p.join("following.json"))
        .unwrap_or_else(|| PathBuf::from("following.json"))
}

pub fn load() -> Following {
    let path = following_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("following.json parse failed ({e}); treating as empty");
            Following::default()
        }),
        Err(_) => Following::default(),
    }
}

pub fn save(f: &Following) -> std::io::Result<()> {
    let path = following_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(f).unwrap_or_default())
}

/// Parse "S03E04" / "s3e4" → (season, episode). Tolerant of case + zero-padding.
pub fn parse_se(s: &str) -> Option<(u32, u32)> {
    let l = s.trim().to_ascii_lowercase();
    let rest = l.strip_prefix('s')?;
    let (sp, ep) = rest.split_once('e')?;
    Some((sp.parse().ok()?, ep.parse().ok()?))
}

pub fn fmt_se(season: u32, episode: u32) -> String {
    format!("S{season:02}E{episode:02}")
}

/// Set (never lower) a show's `watched_through` to SxxExx and persist. Matches by
/// tmdb_id. Returns true if a show was found + updated. `max`-semantics so a
/// re-mark of an older episode can't rewind progress.
pub fn set_watched_through(tmdb_id: u64, season: u32, episode: u32) -> bool {
    let mut f = load();
    let Some(show) = f.shows.iter_mut().find(|s| s.tmdb_id == tmdb_id) else {
        return false;
    };
    let cur = show
        .watched_through
        .as_deref()
        .and_then(parse_se)
        .unwrap_or((0, 0));
    let new = (season, episode);
    if new > cur {
        show.watched_through = Some(fmt_se(season, episode));
        let _ = save(&f);
    }
    true
}

/// Advance a followed show's `watched_through` by TITLE (case-insensitive equality
/// on the cleaned title) to exactly (season, episode) — used when spela itself
/// finishes an episode of a followed show, so watching via spela auto-tracks
/// without a manual mark. `max`-semantics: only ever moves forward, and only to
/// the episode actually finished (never past it). No-op if the title isn't
/// followed. Returns true if a show matched.
pub fn advance_by_title(title: &str, season: u32, episode: u32) -> bool {
    let key = clean_title(title);
    if key.is_empty() {
        return false;
    }
    let mut f = load();
    let Some(show) = f.shows.iter_mut().find(|s| clean_title(&s.title) == key) else {
        return false;
    };
    let cur = show
        .watched_through
        .as_deref()
        .and_then(parse_se)
        .unwrap_or((0, 0));
    if (season, episode) > cur {
        show.watched_through = Some(fmt_se(season, episode));
        let _ = save(&f);
    }
    true
}

/// Lowercase alphanumerics only — tolerant title key ("Rick and Morty" vs
/// "rick & morty" both → "rickandmorty"), so the spela stream title matches the
/// followed-show title without exact-punctuation coupling.
fn clean_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}
