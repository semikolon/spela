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
    let _ = std::fs::copy(
        &path,
        path.with_file_name(format!("following.json.bak-{stamp}")),
    );

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

/// Lowercase alphanumerics only, so the spela stream title matches the followed-show
/// title without exact-punctuation coupling ("The.Boys" / "THE BOYS" / "  The Boys  "
/// all → "theboys").
///
/// CORRECTED 2026-08-28: this used to claim "Rick and Morty" and "rick & morty" both
/// yield "rickandmorty". They do not — `&` is not alphanumeric, so it is DROPPED, giving
/// "rickmorty". Anything spelled out on one side and punctuated on the other ("and" vs
/// "&", "Part Two" vs "II") lands on different keys.
///
/// That is a LATENT hazard rather than a live bug: this is the join between the followed
/// set and the watch ledger, and a miss does not throw — progress silently reads as zero
/// and every episode reappears as new. Verified 2026-08-28 against the real data: all 13
/// followed shows join cleanly, because both sides take the title from TMDB. It would
/// only bite if a title were entered by hand with different punctuation.
#[allow(rustdoc::invalid_html_tags)]
pub fn clean_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `clean_title` is the JOIN KEY between the followed-shows set and the watch
    /// ledger (`derive_watched_through`). A regression here does not throw — it
    /// silently stops matching, so a followed show's progress quietly reads as zero and
    /// every episode reappears as new. Worth pinning tightly.
    #[test]
    fn clean_title_is_a_tolerant_join_key() {
        assert_eq!(clean_title("Rick and Morty"), "rickandmorty");
        // NOT "rickandmorty": `&` is dropped rather than expanded, so a spelled-out
        // conjunction and a punctuated one land on DIFFERENT keys. Pinned deliberately
        // because the docstring used to claim the opposite.
        assert_eq!(clean_title("rick & morty"), "rickmorty");
        assert_ne!(clean_title("Rick & Morty"), clean_title("Rick and Morty"));
        assert_eq!(clean_title("The Boys"), "theboys");
        assert_eq!(clean_title("THE BOYS"), "theboys");
        assert_eq!(clean_title("The.Boys"), "theboys");
        assert_eq!(clean_title("  The Boys  "), "theboys");
        assert_eq!(clean_title("Widow's Bay"), "widowsbay");
        assert_eq!(clean_title("9-1-1"), "911");
    }

    /// Non-ASCII must survive rather than being stripped: a Swedish or accented title
    /// would otherwise collapse toward a different show's key.
    #[test]
    fn clean_title_keeps_non_ascii_letters() {
        assert_eq!(clean_title("Ängelby"), "ängelby");
        assert_eq!(clean_title("Kärlek & Anarki"), "kärlekanarki");
        assert_ne!(clean_title("Ängelby"), clean_title("Angelby"));
    }

    #[test]
    fn parse_se_is_tolerant_of_case_and_padding() {
        assert_eq!(parse_se("S03E04"), Some((3, 4)));
        assert_eq!(parse_se("s3e4"), Some((3, 4)));
        assert_eq!(parse_se(" S03E04 "), Some((3, 4)));
        assert_eq!(parse_se("S10E11"), Some((10, 11)));
        assert_eq!(parse_se("S00E00"), Some((0, 0)));
    }

    #[test]
    fn parse_se_rejects_what_is_not_an_episode_marker() {
        // Returning Some for junk would seed a bogus ledger baseline during migration.
        assert_eq!(parse_se(""), None);
        assert_eq!(parse_se("S03"), None);
        assert_eq!(parse_se("E04"), None);
        assert_eq!(parse_se("SxxEyy"), None);
        assert_eq!(parse_se("3x04"), None);
        assert_eq!(parse_se("season 3"), None);
    }

    #[test]
    fn fmt_se_zero_pads_and_round_trips() {
        assert_eq!(fmt_se(3, 4), "S03E04");
        assert_eq!(fmt_se(10, 11), "S10E11");
        for (s, e) in [(1u32, 1u32), (3, 4), (10, 11), (12, 9)] {
            assert_eq!(parse_se(&fmt_se(s, e)), Some((s, e)));
        }
    }
}

/// Migration + persistence tests. These write real files, so they redirect
/// `SPELA_CONFIG_DIR` (added 2026-08-28 for exactly this) into a temp dir — without it
/// a user-data MIGRATION shipped with zero coverage, because exercising it meant
/// writing to Fredrik's real `following.json`.
///
/// Serialised with a mutex: the override is process-global env, so parallel tests would
/// otherwise redirect each other mid-run.
#[cfg(test)]
mod migration_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spela_follow_{}_{}", name, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn write_v1(dir: &std::path::Path, body: &str) {
        std::fs::write(dir.join("following.json"), body).unwrap();
    }

    #[test]
    fn migration_moves_baselines_into_the_ledger_and_is_idempotent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = scratch("mig");
        let state = scratch("mig_state");
        std::env::set_var("SPELA_CONFIG_DIR", &cfg);
        write_v1(
            &cfg,
            r#"{"schema_version":1,"shows":[{"title":"Silo","tmdb_id":125988,
                "imdb_id":"tt14688458","watched_through":"S03E08"}]}"#,
        );

        migrate_if_needed(&state);

        let f = load();
        assert_eq!(f.schema_version, 2, "schema must advance");
        assert!(
            f.shows[0].watched_through.is_none(),
            "a MIGRATED baseline is consumed, not left behind to be applied twice"
        );
        let app = crate::state::AppState::load(&state);
        assert!(
            app.watched.iter().any(|w| w.key.contains("s03e08")),
            "the baseline must land in the ledger, which is now the SSoT for progress"
        );
        let rows_after_first = app.watched.len();

        migrate_if_needed(&state); // second run must be a no-op
        let app2 = crate::state::AppState::load(&state);
        assert_eq!(
            app2.watched.len(),
            rows_after_first,
            "re-running must not duplicate ledger rows"
        );
        std::env::remove_var("SPELA_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&cfg);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// An unparseable baseline must be PRESERVED, never silently dropped — losing it
    /// would reset a followed show's progress to zero with no trace.
    #[test]
    fn an_unparseable_baseline_is_kept_not_discarded() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = scratch("mig_bad");
        let state = scratch("mig_bad_state");
        std::env::set_var("SPELA_CONFIG_DIR", &cfg);
        write_v1(
            &cfg,
            r#"{"schema_version":1,"shows":[{"title":"Fargo","tmdb_id":60622,
                "watched_through":"whenever"}]}"#,
        );

        migrate_if_needed(&state);

        let f = load();
        assert_eq!(f.schema_version, 2);
        assert_eq!(
            f.shows[0].watched_through.as_deref(),
            Some("whenever"),
            "an unparseable baseline must survive the migration"
        );
        std::env::remove_var("SPELA_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&cfg);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The migration takes a timestamped backup before touching anything.
    #[test]
    fn migration_backs_up_the_original_first() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = scratch("mig_bak");
        let state = scratch("mig_bak_state");
        std::env::set_var("SPELA_CONFIG_DIR", &cfg);
        write_v1(&cfg, r#"{"schema_version":1,"shows":[]}"#);

        migrate_if_needed(&state);

        let backups: Vec<_> = std::fs::read_dir(&cfg)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("following.json.bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "exactly one timestamped backup expected");
        std::env::remove_var("SPELA_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&cfg);
        let _ = std::fs::remove_dir_all(&state);
    }
}
