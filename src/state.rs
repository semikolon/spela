use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::search::SearchResult;

/// Fraction of duration past which the saved resume HWM is cleared.
/// Above this threshold, we assume the user has effectively reached the end
/// (credits or otherwise) and they don't want to auto-resume there next play.
///
/// This is NOT a cleanup threshold — cast_health_monitor relies on the
/// Chromecast's own IDLE signal (real EOF) and the Reaper's duration-aware
/// grace period to tear streams down. Apr 19, 2026: raised from 0.92 after
/// Send Help (113 min) had its stream killed at 1:43:54 with 8:42 of climax
/// remaining. 92% was too early for modern films whose credits start at 96-99%.
pub const HWM_CLEAR_FRACTION: f64 = 0.96;

/// Absolute tail window: within this many seconds of the end, clear the HWM
/// too (covers short-form content where a percentage threshold lands only a
/// few seconds from EOF).
pub const HWM_CLEAR_TAIL_SECS: f64 = 300.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub current: Option<CurrentStream>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub preferences: Preferences,
    /// Apr 29, 2026: items queued to auto-fire when the current stream
    /// reaches natural EOF (HWM past HWM_CLEAR_FRACTION). FIFO. The
    /// cast_health_monitor pops the front entry after `do_cleanup` and
    /// spawns a self-call to `/play` with its fields.
    #[serde(default)]
    pub queue: Vec<QueuedItem>,
    /// Resume positions by IMDB ID (seconds). Used by Custom Cast Receiver.
    #[serde(default)]
    pub resume_positions: HashMap<String, f64>,
    /// Apr 30, 2026: source-file paths flagged as corrupt by
    /// `transcode::inspect_ffmpeg_log_for_corruption` (Hijack S02E05
    /// MeGusta-class incident). `do_cleanup` populates; Local Bypass
    /// skips matches that are in this set. Set, not Vec, so dedup is
    /// free and lookup is O(1).
    #[serde(default)]
    pub corrupt_files: std::collections::HashSet<String>,
}

/// Apr 29, 2026: a queued play request for auto-firing when the current
/// stream reaches natural EOF. Subset of PlayRequest fields — sufficient
/// for `do_play` to reconstruct everything via last_search auto-fill OR
/// directly from the magnet + metadata captured here at queue-time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedItem {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cast_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default)]
    pub smooth: bool,
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
    /// Apr 28, 2026: TMDB poster URL (full, prefixed with image base) for
    /// the currently-playing show/movie. Plumbed through to `cast.rs`
    /// `CastMetadata` so the receiver renders the rich-UI player. Persists
    /// across server restarts so a recast (cast_health_monitor's auto-recover
    /// path) can re-issue LOAD with the same metadata. Optional + serde
    /// default for forward compat with state.json files written before this
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
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
    #[serde(default)]
    pub smooth: bool,
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
            queue: Vec::new(),
            corrupt_files: std::collections::HashSet::new(),
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
    /// If duration is provided and t is near the end (>= HWM_CLEAR_FRACTION or within
    /// HWM_CLEAR_TAIL_SECS of the end), clears the saved position so next play starts
    /// fresh instead of auto-resuming from the credits.
    pub fn save_position_smart(&mut self, imdb_id: Option<String>, title: Option<String>, t: f64, duration: Option<f64>) -> (String, bool) {
        let key = match resume_position_key(imdb_id.as_deref(), title.as_deref()) {
            Some(k) => k,
            None => return ("unknown".into(), false),
        };

        // --- Completion Logic ---
        // Clear the HWM when the user has effectively watched to the end.
        if let Some(dur) = duration {
            if dur > 0.0 && (t >= dur * HWM_CLEAR_FRACTION || t >= (dur - HWM_CLEAR_TAIL_SECS)) {
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
        if let Some(key) = resume_position_key(imdb_id.as_deref(), title.as_deref()) {
            return self.resume_positions.get(&key).copied().unwrap_or(0.0);
        }
        0.0
    }

    /// Force reset a resume position (bypasses High-Water Mark).
    pub fn reset_position(&mut self, imdb_id: Option<String>, title: Option<String>) -> String {
        let key = match resume_position_key(imdb_id.as_deref(), title.as_deref()) {
            Some(k) => k,
            None => return "unknown".into(),
        };
        self.resume_positions.remove(&key);
        key
    }
}

/// Build the resume-position key for a movie or TV episode.
///
/// Apr 15, 2026 fix: TV episodes share the SHOW's imdb_id (e.g. `tt1190634`
/// is The Boys as a whole, not S05E03 specifically). Keying resume_positions
/// by raw imdb_id made every episode of the same show collide: finishing
/// S05E02 near its 92% threshold wrote 3437s under `tt1190634`, and the next
/// night's S05E03 auto-resume picked up that stale HWM → started the new
/// episode at minute 57. This helper parses an `SxxExx` marker out of the
/// title whenever present and appends it to the imdb_id so each episode
/// gets its own bucket. Movies (no S/E marker) keep the raw imdb_id.
///
/// Order of preference:
///   1. `imdb_id + "_sXXeYY"` when title contains a parseable SxxExx marker
///   2. `imdb_id` alone when the title has no S/E marker (movie)
///   3. `slugify(title)` when no imdb_id is available
///   4. `None` when neither imdb_id nor title is usable
fn resume_position_key(imdb_id: Option<&str>, title: Option<&str>) -> Option<String> {
    if let Some(id) = imdb_id.filter(|s| !s.is_empty()) {
        let suffix = title.and_then(extract_se_suffix).unwrap_or_default();
        return Some(format!("{}{}", id, suffix));
    }
    if let Some(t) = title.filter(|s| !s.is_empty()) {
        return Some(slugify(t));
    }
    None
}

/// Extract `_sXXeYY` suffix from a TV release title, or `None` if the
/// title doesn't contain a recognizable SxxExx pattern.
///
/// Accepts both zero-padded and unpadded forms (`S05E03`, `S5E3`, `s5e03`).
/// Returns a canonical zero-padded suffix for consistent keying across
/// different release-name conventions.
fn extract_se_suffix(title: &str) -> Option<String> {
    let lower = title.to_lowercase();
    let bytes = lower.as_bytes();
    let n = bytes.len();
    // Walk the string scanning for `s<digits>e<digits>` patterns. We want
    // the FIRST occurrence — scene release names sometimes have a year that
    // includes digits, but the SxxExx marker comes after the show name in
    // torrent naming conventions and is unambiguous once we anchor on 's'.
    let mut i = 0;
    while i + 3 < n {
        if bytes[i] == b's' && bytes[i + 1].is_ascii_digit() {
            // Require a word boundary before the 's' to avoid matching the
            // 's' at the end of a show name like "Lost".
            let boundary_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if boundary_ok {
                let mut j = i + 1;
                while j < n && bytes[j].is_ascii_digit() { j += 1; }
                if j < n && bytes[j] == b'e' && j + 1 < n && bytes[j + 1].is_ascii_digit() {
                    let mut k = j + 1;
                    while k < n && bytes[k].is_ascii_digit() { k += 1; }
                    let season: u32 = lower[i + 1..j].parse().ok()?;
                    let episode: u32 = lower[j + 1..k].parse().ok()?;
                    return Some(format!("_s{:02}e{:02}", season, episode));
                }
            }
        }
        i += 1;
    }
    None
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

        // 1. Regular save (50 mins in) — well below HWM_CLEAR_FRACTION
        let (key, saved) = state.save_position_smart(imdb_id.clone(), title.clone(), 3000.0, Some(6907.0));
        assert!(saved);
        assert_eq!(state.resume_positions.get(&key), Some(&3000.0));

        // 2. Apr 19, 2026 regression pin: at 95.5% (6600s / 6907s = 0.9555),
        //    below the 0.96 HWM_CLEAR_FRACTION threshold, the HWM must SURVIVE
        //    — the Send Help incident was caused by killing streams past 92%
        //    when the user was still mid-climax. This case must save, not clear.
        let (key2, saved2) = state.save_position_smart(imdb_id.clone(), title.clone(), 6600.0, Some(6907.0));
        assert!(saved2);
        assert_eq!(state.resume_positions.get(&key2), Some(&6600.0),
                   "HWM must survive at 95.5% — below HWM_CLEAR_FRACTION (0.96)");

        // 3. Completion clear at 97.1% (6708s / 6907s) — past HWM_CLEAR_FRACTION.
        let (key3, saved3) = state.save_position_smart(imdb_id.clone(), title.clone(), 6708.0, Some(6907.0));
        assert!(saved3);
        assert_eq!(state.resume_positions.get(&key3), None,
                   "HWM must clear at 97.1% — past HWM_CLEAR_FRACTION");
    }

    /// Apr 19, 2026: the HWM clear threshold must be at least 0.96. Lower values
    /// kill real content — Send Help (113 min) would have its HWM cleared at
    /// 1:43:54, with 8:42 of climax remaining, making it impossible to finish.
    /// The 0.96 lower bound is load-bearing UX policy — don't regress it.
    #[test]
    fn test_hwm_clear_fraction_invariant() {
        assert!(HWM_CLEAR_FRACTION >= 0.96,
                "HWM_CLEAR_FRACTION must not drop below 0.96 — see Send Help incident \
                 Apr 19, 2026. Lower values amputate climax scenes of modern films.");
        assert!(HWM_CLEAR_FRACTION < 1.0,
                "HWM_CLEAR_FRACTION must be < 1.0 or the HWM never clears.");
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

    /// Apr 15, 2026 regression pin: TV episodes of the SAME show (sharing an
    /// imdb_id) must not collide in resume_positions. This is the bug that
    /// made S05E03 auto-resume at minute 57 because S05E02's final save
    /// used the same `tt1190634` key. Fixed by appending `_sXXeYY` suffix
    /// parsed from the release title.
    #[test]
    fn test_tv_episodes_do_not_collide_on_shared_imdb_id() {
        let mut state = AppState::default();
        let show_id = Some("tt1190634".to_string()); // The Boys

        // Save S05E02 at minute 57 (close to end but not yet 92%)
        let dur = 3753.0; // ~63 min episode
        let (s02_key, _) = state.save_position_smart(
            show_id.clone(),
            Some("The Boys S05E02".to_string()),
            3437.0,
            Some(dur),
        );

        // Save S05E03 at minute 2 of a 66-min episode
        let (s03_key, _) = state.save_position_smart(
            show_id.clone(),
            Some("The Boys S05E03".to_string()),
            120.0,
            Some(3947.0),
        );

        // Keys MUST be different — otherwise one save overwrites the other.
        assert_ne!(s02_key, s03_key);
        assert!(s02_key.contains("s05e02"), "S02 key missing suffix: {s02_key}");
        assert!(s03_key.contains("s05e03"), "S03 key missing suffix: {s03_key}");

        // Both positions must be independently retrievable
        let s02_pos = state.get_position(show_id.clone(), Some("The Boys S05E02".to_string()));
        let s03_pos = state.get_position(show_id.clone(), Some("The Boys S05E03".to_string()));
        assert_eq!(s02_pos, 3437.0);
        assert_eq!(s03_pos, 120.0);

        // Resetting one must NOT affect the other
        state.reset_position(show_id.clone(), Some("The Boys S05E02".to_string()));
        let s02_after = state.get_position(show_id.clone(), Some("The Boys S05E02".to_string()));
        let s03_after = state.get_position(show_id.clone(), Some("The Boys S05E03".to_string()));
        assert_eq!(s02_after, 0.0, "S02 should be cleared");
        assert_eq!(s03_after, 120.0, "S03 must survive S02's reset");
    }

    /// Movies (no SxxExx in title) keep using the raw imdb_id as key, so
    /// the bug fix for TV shows doesn't regress movie behavior.
    #[test]
    fn test_movies_use_raw_imdb_id_key() {
        let key = resume_position_key(Some("tt10548174"), Some("28 Years Later (2025)")).unwrap();
        assert_eq!(key, "tt10548174");

        // Same id + different marketing titles → same key (correct for movies)
        let key_alt = resume_position_key(Some("tt10548174"), Some("28 Years Later — Extended")).unwrap();
        assert_eq!(key, key_alt);
    }

    /// extract_se_suffix test corpus — handle all the real naming conventions
    /// spela encounters from Torrentio (playWEB, FLUX, etc.) plus edge cases.
    #[test]
    fn test_extract_se_suffix_variants() {
        // Zero-padded forms (most common)
        assert_eq!(extract_se_suffix("The Boys S05E03 Every One of You Sons of Bitches"),
                   Some("_s05e03".to_string()));
        assert_eq!(extract_se_suffix("The.Boys.S05E02.Teenage.Kix.1080p"),
                   Some("_s05e02".to_string()));
        // Case-insensitive
        assert_eq!(extract_se_suffix("the.boys.s05e03.flux"), Some("_s05e03".to_string()));
        // Unpadded single-digit forms — normalize to 2-digit
        assert_eq!(extract_se_suffix("Lost S1E1 Pilot"), Some("_s01e01".to_string()));
        // Double-digit episodes
        assert_eq!(extract_se_suffix("Show S1E12 Finale"), Some("_s01e12".to_string()));
        // Space-separated (playWEB-style release names)
        assert_eq!(extract_se_suffix("The Boys S05E03 Every One"), Some("_s05e03".to_string()));
        // No SxxExx marker → None
        assert_eq!(extract_se_suffix("28 Years Later (2025)"), None);
        assert_eq!(extract_se_suffix("The Matrix 1999 1080p"), None);
        // Empty string
        assert_eq!(extract_se_suffix(""), None);
        // Word-boundary guard: "ses" should NOT trigger on the trailing 's'
        // (this was a real pitfall in earlier drafts — "Loses"/"Dragons" etc).
        assert_eq!(extract_se_suffix("Dragons S05E01"), Some("_s05e01".to_string()));
        assert_eq!(extract_se_suffix("NoSeOrEHere"), None);
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
            poster_url: None,
            ss_offset: 1800.0,
            smooth: false,
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

        // The HWM_CLEAR_FRACTION threshold on a 3823.6s episode is ~3670s.
        let duration = 3823.6_f64;
        assert!(absolute < duration * HWM_CLEAR_FRACTION);
        // At 3700s absolute (~96.8%), we should be past the threshold.
        let near_end = 3700.0_f64;
        assert!(near_end >= duration * HWM_CLEAR_FRACTION);
    }
}
