//! Recommender store (roadmap slice 3 harness + slice 7 unified queue).
//!
//! `~/.config/spela/recommendations.json` (user-local, next to config.toml — the
//! public repo NEVER carries watch/taste data) holds a ranked list of "what to
//! watch next" picks CURATED BY THE LLM HARNESS. Per the arsenal-and-harness
//! pattern, spela is the ARSENAL (this store + the read/write endpoints + the
//! queue that surfaces it); the HARNESS is Claude/CC (Phase 1 — reads
//! `taste_profile.md` + `/watched` + `/watchlist` + air-dates, ranks, POSTs the
//! picks here). Model-swappable later (a Phase-2 autonomous harness writes the
//! same file). Each pick carries a one-line `why` — the rationale IS the value.
//!
//! Deterministic on spela's side (spela never invents picks); the reasoning lives
//! entirely in the harness. Serve-time excludes already-seen titles by joining the
//! watch-ledger, so a pick written before Fredrik watched it drops off on its own.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One curated pick. `why` is the harness's one-line rationale (e.g. "grounded,
/// tense sci-fi like Silo which you loved — 96%"); it's what makes the row feel
/// wise rather than a generic list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecEntry {
    pub title: String,
    /// "movie" | "tv" — routes the tap to the right search mode.
    #[serde(default = "default_media_type")]
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    /// Critic score (RT/MDBList), 0-100, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rt_score: Option<u32>,
    /// The harness's one-line reason this is recommended now.
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    /// Rank (0 = top). Serve order; the harness sets it.
    #[serde(default)]
    pub rank: u32,
}

fn default_media_type() -> String {
    "movie".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recommendations {
    #[serde(default)]
    pub picks: Vec<RecEntry>,
    /// When the harness last wrote this list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<DateTime<Utc>>,
    /// Harness identity (e.g. "claude-opus-4.8") — provenance, model-swappable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
}

/// `~/.config/spela/recommendations.json` — same hardcoded-XDG resolution as
/// `following.json` (avoids macOS's Application Support).
pub fn recommendations_path() -> PathBuf {
    crate::config::Config::config_path()
        .parent()
        .map(|p| p.join("recommendations.json"))
        .unwrap_or_else(|| PathBuf::from("recommendations.json"))
}

pub fn load() -> Recommendations {
    match std::fs::read_to_string(recommendations_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("recommendations.json parse failed ({e}); treating as empty");
            Recommendations::default()
        }),
        Err(_) => Recommendations::default(),
    }
}

pub fn save(r: &Recommendations) -> std::io::Result<()> {
    let path = recommendations_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(r).unwrap_or_default())
}

/// Replace the whole list with a freshly-curated set, stamping provenance. The
/// harness POSTs the full ranked list (not incremental) — curation is a whole-list
/// judgement, so a replace is the honest write. Picks are sorted by `rank` so the
/// serve order is deterministic regardless of POST order.
pub fn set_picks(mut picks: Vec<RecEntry>, generated_by: Option<String>) -> std::io::Result<usize> {
    picks.sort_by_key(|p| p.rank);
    let count = picks.len();
    save(&Recommendations {
        picks,
        generated_at: Some(Utc::now()),
        generated_by,
    })?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendations_json_roundtrip_preserves_all_fields() {
        let recs = Recommendations {
            picks: vec![RecEntry {
                title: "Poor Things".into(),
                media_type: "movie".into(),
                tmdb_id: Some(792307),
                imdb_id: Some("tt14230458".into()),
                year: Some(2023),
                rt_score: Some(92),
                why: "surreal + your taste for the strange".into(),
                poster_url: Some("http://p/x.jpg".into()),
                rank: 1,
            }],
            generated_at: None,
            generated_by: Some("claude".into()),
        };
        let back: Recommendations =
            serde_json::from_str(&serde_json::to_string(&recs).unwrap()).unwrap();
        assert_eq!(back.picks.len(), 1);
        assert_eq!(back.picks[0].title, "Poor Things");
        assert_eq!(back.picks[0].rt_score, Some(92));
        assert_eq!(back.picks[0].rank, 1);
        assert_eq!(back.generated_by.as_deref(), Some("claude"));
    }

    #[test]
    fn minimal_harness_json_deserializes_with_defaults() {
        // The harness contract: a pick POSTed with only title + why must parse, with
        // media_type→"movie", rank→0, and all optionals absent — so a lean write
        // never fails to load.
        let back: Recommendations =
            serde_json::from_str(r#"{"picks":[{"title":"Sinners","why":"loved the vibe"}]}"#)
                .unwrap();
        assert_eq!(back.picks.len(), 1);
        assert_eq!(back.picks[0].media_type, "movie");
        assert_eq!(back.picks[0].rank, 0);
        assert_eq!(back.picks[0].tmdb_id, None);
        assert_eq!(back.picks[0].why, "loved the vibe");
    }
}
