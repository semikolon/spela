//! Followed-series tracking (roadmap slice 2b, "Up Next"). TVTime replacement.
//!
//! A user-local `~/.config/spela/following.json` is a thin MEMBERSHIP set of the
//! ongoing series Fredrik follows (title + tmdb_id + imdb_id). It stores NO
//! progress — how far he's watched each show is DERIVED from the watch-ledger (the
//! single source of truth: `AppState::derive_watched_through`), so unfollowing a
//! show can't lose his place, and re-following resumes exactly where he left off.
//! spela joins the set with TMDB air-dates (`SearchEngine::tv_status`) to compute,
//! per show, the next-unwatched episode + how many aired episodes are new.
//! `migrate_if_needed` performs the one-time move of legacy inline
//! `watched_through` baselines into the ledger.
//!
//! Deterministic — no LLM. Personal data → never committed (public repo); lives
//! only on the spela host next to `config.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowedShow {
    pub title: String,
    pub tmdb_id: u64,
    /// IMDb id (e.g. "tt14688458"), captured when the show is followed from a
    /// search result. Lets `AppState::derive_watched_through` join the ledger
    /// EXACTLY; absent (legacy / migrated) entries fall back to a cleaned-title
    /// match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    /// LEGACY (schema v1): the old inline progress baseline. Migrated into the
    /// watch-ledger by `migrate_if_needed`, then left None — progress is DERIVED
    /// from the ledger now, never stored here (so unfollow can't lose it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watched_through: Option<String>,
    /// Whole seasons the user has marked seen OUTSIDE the linear `watched_through`
    /// baseline (e.g. saw Fargo S2-5 but not S1). The single caught-up HWM assumes
    /// start-to-finish viewing and can't represent a middle-seen gap; the
    /// New-Episodes count subtracts these seasons so only genuinely-unseen episodes
    /// are counted. Coarse (per-season) by design — a per-episode overlay would just
    /// duplicate the ledger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen_seasons: Vec<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Following {
    #[serde(default)]
    pub shows: Vec<FollowedShow>,
    /// 1 (or absent) = legacy inline `watched_through`; 2 = progress lives in the
    /// ledger. Gates the one-time `migrate_if_needed`.
    #[serde(default)]
    pub schema_version: u32,
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

/// Add a show to the followed set (MEMBERSHIP only — progress derives from the
/// ledger). Idempotent by tmdb_id; back-fills `imdb_id` if newly known. Returns
/// true if the set changed.
pub fn add_show(title: &str, tmdb_id: u64, imdb_id: Option<String>) -> bool {
    let mut f = load();
    if let Some(s) = f.shows.iter_mut().find(|s| s.tmdb_id == tmdb_id) {
        if s.imdb_id.is_none() && imdb_id.as_deref().is_some_and(|v| !v.is_empty()) {
            s.imdb_id = imdb_id;
            let _ = save(&f);
            return true;
        }
        return false;
    }
    f.shows.push(FollowedShow {
        title: title.to_string(),
        tmdb_id,
        imdb_id: imdb_id.filter(|v| !v.is_empty()),
        watched_through: None,
        seen_seasons: Vec::new(),
    });
    let _ = save(&f);
    true
}

/// Set the whole-seasons-seen overlay for a followed show (replace semantics —
/// the UI sends the full checked set). Returns true if the show exists.
pub fn set_seen_seasons(tmdb_id: u64, mut seasons: Vec<u32>) -> bool {
    let mut f = load();
    if let Some(s) = f.shows.iter_mut().find(|s| s.tmdb_id == tmdb_id) {
        seasons.sort_unstable();
        seasons.dedup();
        s.seen_seasons = seasons;
        let _ = save(&f);
        true
    } else {
        false
    }
}

/// Remove a show from the followed set. MEMBERSHIP only — the ledger (and thus all
/// watched progress) is untouched, so re-following later resumes exactly where it
/// left off. Returns true if a show was removed.
pub fn remove_show(tmdb_id: u64) -> bool {
    let mut f = load();
    let before = f.shows.len();
    f.shows.retain(|s| s.tmdb_id != tmdb_id);
    if f.shows.len() != before {
        let _ = save(&f);
        true
    } else {
        false
    }
}

/// One-time migration: move the pre-spela `watched_through` baselines OUT of
/// following.json and INTO the watch-ledger (the new single source of truth), so
/// unfollowing a show can no longer lose progress. Idempotent (gated on
/// `schema_version`), fail-safe (a baseline that can't be parsed is kept + logged,
/// never silently dropped), and backs up following.json first. Network-free: seeds
/// are title-keyed, so `derive_watched_through` matches them by cleaned title (imdb
/// linkage fills in as real completions land / shows are re-added from search).
pub fn migrate_if_needed(state_dir: &std::path::Path) {
    let mut f = load();
    if f.schema_version >= 2 {
        return;
    }
    let path = following_path();
    let stamp = chrono::Utc::now().timestamp();
    let _ = std::fs::copy(&path, path.with_file_name(format!("following.json.bak-{stamp}")));

    let sd = state_dir.to_path_buf();
    let mut app = crate::state::AppState::load(&sd);
    let mut ledger_changed = false;
    for s in &mut f.shows {
        let Some(wt) = s.watched_through.take() else {
            continue;
        };
        match parse_se(&wt) {
            Some((season, episode)) => {
                let title = format!("{} {}", s.title, fmt_se(season, episode));
                if app.mark_watched_seed(s.imdb_id.clone(), title) {
                    ledger_changed = true;
                }
            }
            None => {
                tracing::warn!(
                    "following migration: couldn't parse watched_through {wt:?} for {:?} — baseline left unmigrated",
                    s.title
                );
                s.watched_through = Some(wt); // never silently drop it
            }
        }
    }
    if ledger_changed {
        let _ = app.save(&sd);
    }
    f.schema_version = 2;
    match save(&f) {
        Ok(()) => tracing::info!("following migration → v2 complete (baselines moved to ledger)"),
        Err(e) => tracing::error!("following migration: failed to write following.json v2: {e}"),
    }
}

/// Lowercase alphanumerics only — tolerant title key ("Rick and Morty" vs
/// "rick & morty" both → "rickandmorty"), so the spela stream title matches the
/// followed-show title without exact-punctuation coupling.
pub fn clean_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}
