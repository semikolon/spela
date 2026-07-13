# spela as watch-tracker + recommender — roadmap

**Why:** TV Time shuts down 2026-07-15, and every polished tracker (Hobi, Simkl, …)
is ads-or-subscription. spela already plays everything, knows what you play, and
stores per-episode resume state — so it can *be* the tracker: free, ad-free, no
subscription, yours, and integrated with search/play + the recommender. This
replaces the external-tracker migration entirely.

**Data placement (public-repo rule):** the spela repo is PUBLIC. All personal
watch/taste data is user-local under `~/.config/spela/` (like `config.toml`):
`taste_profile.md`, `tvtime_watchlist_2026-07-13.md`, and (future) `watchlist.json`
/ `watched.json`. Only non-personal feature design lives in the repo.

## Slices

1. **Continue-watching row — ✅ SHIPPED (2026-07-13, `4b397c0`).**
   `GET /recent` = distinct titles played in the last 14 days (deduped by cleaned
   title, newest-first) from spela's own `history`. Web remote renders a chip row
   beneath the search bar on the landing view; tap → jump to that show's sources.
   Labels cleaned via `clean_title_for_tmdb`. Hides when history is empty.

2. **Watched-ledger (spela-native tracking) — NEXT.** Auto-mark an episode/movie
   "watched" when a play passes ~90% (spela already knows play + position). Persist
   to `~/.config/spela/watched.json` keyed like `resume_position_key`. This is the
   tracker's spine; the /recent row and next-up derive from it. Add a "My Shows /
   Up Next" view (next unwatched episode per followed show, via TMDB episode lists).

3. **To-watch list + recommender feed.** `watchlist.json` the recommender appends
   to; a UI view. Recommender = CC-first LLM-with-tools harness (DIM-style) reading
   `taste_profile.md` + tools (TMDB, critics scores, availability, watched.json).
   Meta-rules from the taste profile: RT ≥88% gate, mood/bandwidth match, tone-first.

4. **RT watchlist import.** RT has no public API → one-time scrape/import of the RT
   watchlist into `watchlist.json`; NOT continuous auto-sync (fragile). Trakt/
   Letterboxd have real APIs if auto-sync is ever wanted.

5. **Auto-track any house Chromecast (non-spela casts) — later, feasible.** Poll
   each Chromecast's media status (rust_cast/pychromecast) for app + title/series/
   episode; match to TMDB; mark watched. Caveat: metadata quality varies by app
   (Netflix/Disney report well; some report little) → partial but real. Self-
   referential-safety: read-only polling, no control actions.

## Notes
- TV Time GDPR export is a dead end for history (verified 2026-07-13: 3 byte-identical
  exports, all account/settings/IP only, zero watch data). Seed initial progress from
  `~/.config/spela/tvtime_watchlist_2026-07-13.md` (the screenshots).
- Future spela iOS app (if built) is for playing/remote, not re-tracking — the tracker
  lives in the shared backend + the existing web-remote PWA.
