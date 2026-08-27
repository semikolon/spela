//! Per-show intel notes: the one-liner that says what people are saying about a show
//! right now, or what the talk is about whether it comes back.
//!
//! WHY THIS IS A STORE AND NOT AN API CALL. A show's *status* — returning, ended,
//! cancelled — is a fact TMDB and TVmaze both hold, so spela computes it. Its
//! *reception* and the *talk about its future* are neither structured nor stable: they
//! change week to week, live in reviews and trade reporting, and no free API exposes
//! them. That is the arsenal-and-harness split the recommender already uses — spela is
//! the arsenal (it stores, serves and dates the note), and the LLM harness writes it.
//!
//! Every note carries the date it was written, and the UI shows that date once the note
//! is old, so stale intel reads AS stale rather than as current fact. A note nobody has
//! refreshed in six months is worse than no note if it looks fresh.
//!
//! User-local on Darwin (`~/.config/spela/show_notes.json`), never the public repo — it
//! carries opinions about what Fredrik is watching.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShowNote {
    /// The one-liner itself. Small font, one line, no trailing period needed.
    pub note: String,
    /// ISO date the note was written. Drives the staleness label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Optional human-readable provenance ("Variety", "critics", "showrunner").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShowNotes {
    /// Keyed by imdb id when known, else by normalized title. Both are looked up.
    #[serde(default)]
    pub notes: HashMap<String, ShowNote>,
}

pub fn notes_path() -> PathBuf {
    crate::config::Config::config_path()
        .parent()
        .map(|p| p.join("show_notes.json"))
        .unwrap_or_else(|| PathBuf::from("show_notes.json"))
}

/// Normalize a title into a lookup key. Mirrors the ledger's join so a note written
/// against a title still matches when the caller has a slightly different casing or
/// punctuation.
pub fn title_key(title: &str) -> String {
    crate::following::clean_title(title).to_lowercase()
}

pub fn load() -> ShowNotes {
    let p = notes_path();
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(n: &ShowNotes) -> Result<()> {
    let p = notes_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(n)?)?;
    Ok(())
}

impl ShowNotes {
    /// Look up by imdb id first, then by normalized title. An id match is exact; the
    /// title fallback exists because a note may be written before an id is known.
    pub fn get(&self, imdb_id: Option<&str>, title: &str) -> Option<&ShowNote> {
        if let Some(id) = imdb_id.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(n) = self.notes.get(id) {
                return Some(n);
            }
        }
        self.notes.get(&title_key(title))
    }

    pub fn set(&mut self, key: &str, note: ShowNote) {
        let k = if key.starts_with("tt") {
            key.to_string()
        } else {
            title_key(key)
        };
        if note.note.trim().is_empty() {
            self.notes.remove(&k);
        } else {
            self.notes.insert(k, note);
        }
    }
}

/// Turn TMDB's and TVmaze's status strings into the one word a viewer actually wants.
///
/// The two sources are complementary rather than redundant: TMDB distinguishes
/// `Canceled` from `Ended`, which is the difference between a show killed off and one
/// that finished on its own terms; TVmaze is quicker to reflect reality in general,
/// which is why this codebase already prefers it for air dates. Where they disagree the
/// more specific verdict wins, and an ending is believed from either.
pub fn describe_status(tmdb_status: Option<&str>, tvmaze_status: Option<&str>) -> Option<String> {
    let t = tmdb_status.unwrap_or("").trim();
    let v = tvmaze_status.unwrap_or("").trim();
    let out = match () {
        _ if t.eq_ignore_ascii_case("Canceled") || t.eq_ignore_ascii_case("Cancelled") => {
            "Cancelled"
        }
        _ if t.eq_ignore_ascii_case("Ended") || v.eq_ignore_ascii_case("Ended") => "Ended",
        _ if v.eq_ignore_ascii_case("To Be Determined") => "Renewal undecided",
        _ if t.eq_ignore_ascii_case("Returning Series") || v.eq_ignore_ascii_case("Running") => {
            "Returning"
        }
        _ if t.eq_ignore_ascii_case("In Production") || t.eq_ignore_ascii_case("Planned") => {
            "In production"
        }
        _ => return None,
    };
    Some(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_beats_a_generic_ended() {
        // TVmaze only knows the show stopped; TMDB knows it was killed. The viewer
        // cares about the difference.
        assert_eq!(
            describe_status(Some("Canceled"), Some("Ended")).as_deref(),
            Some("Cancelled")
        );
    }

    #[test]
    fn an_ending_is_believed_from_either_source() {
        assert_eq!(
            describe_status(Some("Returning Series"), Some("Ended")).as_deref(),
            Some("Ended")
        );
        assert_eq!(
            describe_status(Some("Ended"), Some("Running")).as_deref(),
            Some("Ended")
        );
    }

    #[test]
    fn awaiting_renewal_is_its_own_state() {
        // The question "will it come back" has three answers, not two.
        assert_eq!(
            describe_status(None, Some("To Be Determined")).as_deref(),
            Some("Renewal undecided")
        );
    }

    #[test]
    fn running_shows_read_as_returning() {
        assert_eq!(
            describe_status(Some("Returning Series"), Some("Running")).as_deref(),
            Some("Returning")
        );
    }

    #[test]
    fn nothing_known_yields_no_badge() {
        assert_eq!(describe_status(None, None), None);
        assert_eq!(describe_status(Some(""), Some("")), None);
    }

    #[test]
    fn lookup_prefers_the_id_then_falls_back_to_title() {
        let mut n = ShowNotes::default();
        n.set(
            "tt1234567",
            ShowNote {
                note: "by id".into(),
                ..Default::default()
            },
        );
        n.set(
            "The Show",
            ShowNote {
                note: "by title".into(),
                ..Default::default()
            },
        );
        assert_eq!(n.get(Some("tt1234567"), "Whatever").unwrap().note, "by id");
        assert_eq!(n.get(None, "the show").unwrap().note, "by title");
        assert_eq!(
            n.get(Some("tt0000000"), "The Show").unwrap().note,
            "by title"
        );
    }

    #[test]
    fn an_empty_note_clears_rather_than_stores_a_blank() {
        let mut n = ShowNotes::default();
        n.set(
            "tt1",
            ShowNote {
                note: "x".into(),
                ..Default::default()
            },
        );
        n.set(
            "tt1",
            ShowNote {
                note: "  ".into(),
                ..Default::default()
            },
        );
        assert!(n.get(Some("tt1"), "").is_none());
    }
}
