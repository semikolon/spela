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

1. **Recently-watched row — ✅ SHIPPED (2026-07-13, `4b397c0`/`68c1b84`).**
   `GET /recent` = distinct titles played in the last 14 days (deduped on cleaned
   title via `clean_title_for_tmdb`, newest-first) from spela's own `history`. Web
   remote renders a **wrapping** chip row beneath the search bar on the landing
   view; tap → jump to that show's sources (mode auto-set TV/movie). Hides when
   history is empty. **NOT episode-resume** — it's "jump back into a show you
   watched recently"; the earlier "Continue watching" label over-promised (a
   finished episode can't be continued), so it's now "Recently watched". True
   next-unwatched resume = slice 2b.

2. **Watched-ledger — ✅ SHIPPED (2026-07-13, `f22793f`).** `AppState.watched:
   Vec<WatchedEntry>` keyed like `resume_positions`. `mark_watched()` rides
   `save_position_smart`'s existing completion branch (`t >= HWM_CLEAR_FRACTION`),
   so any play watched to the end is auto-recorded — zero manual tracking. `GET
   /watched` exposes it (newest-first). Lives in `state.json` (user-local). This is
   the tracker's spine; the recommender's seen-check + slice 2b derive from it.

2b. **"Up Next" view — NEXT.** Per followed show, the next UNWATCHED episode
   (watched-ledger + TMDB episode list + airdates). This is the real "continue
   watching" the row can't do yet. Surfaces "new episode aired" too (TMDB
   airdates → notification, like TV Time's reminders).

3. **To-watch list + recommender — arsenal ✅ SHIPPED (2026-07-13, `b4fed13`); harness (curation) = Claude, ongoing.**
   `GET /watchlist` serves `~/.config/spela/watchlist.json` (seeded from the RT
   import). A "To Watch" nav tab + view lists series + movies as tappable rows
   (RT score shown, <88 dimmed per the gate; already-watched hidden); tap → search
   that title. That's the ARSENAL. The HARNESS = Claude curating/ranking the list
   (reading `taste_profile.md`); still ongoing (Phase 1).
   **The LLM harness IS Claude/CC now (Phase 1), exactly like DIM** (Fredrik: "the
   LLM is YOU until I say otherwise"). So spela builds the ARSENAL (the to-watch
   store + endpoints + a "To Watch" UI view + the tools: `/search`+TMDB, critics
   scores, `/watched` seen-check, `taste_profile.md`) and Claude is the reasoning
   harness that curates/ranks the list — NOT an external LLM API wired into spela.
   Keep it model-swappable (Phase 2 = a `claude -p` / Gemini harness) per the
   arsenal-and-harness doc; `claude -p` CC-wrap is not the long-term multi-user
   harness (ToS). Meta-rules (from `~/.config/spela/taste_profile.md`): RT ≥88%
   gate (with genre-override exceptions), mood/bandwidth match, tone-first,
   exclude-already-watched (watched-ledger + the taste profile's "watched & loved"
   list).

4. **RT watchlist import — ✅ DONE (2026-07-13, manual paste).** RT has no public
   API, so Fredrik pasted his watchlist → `~/.config/spela/rt_watchlist_2026-07-13.md`
   (movies + series, RT crit/aud scores, availability). This seeds slice 3's
   to-watch list. NOT continuous auto-sync (fragile). NOTE: the RT "watchlist"
   MIXES to-watch with already-watched-loved — seen-status must be confirmed, not
   assumed.

5. **Auto-track any house Chromecast (non-spela casts) — later, feasible.** Poll
   each Chromecast's media status (rust_cast/pychromecast) for app + title/series/
   episode; match to TMDB; mark watched. Caveat: metadata quality varies by app
   (Netflix/Disney report well; some report little) → partial but real. Self-
   referential-safety: read-only polling, no control actions.

## Notes
- **⚠ Arsenal data lives on the SPELA HOST (Darwin `~/.config/spela/`), not the Mac.**
  spela serves `watchlist.json` from the host it runs on (Darwin). Authoring these
  files on the Mac (this repo's dev box) does NOT reach spela — `/watchlist` reads
  Darwin's copy. Edit on Darwin, or `scp` the file over after editing. (Bit us
  2026-07-13: wrote `watchlist.json` on the Mac, `/watchlist` returned empty until
  scp'd to Darwin.) `taste_profile.md` + `rt_watchlist_*.md` are the Claude-harness's
  inputs — keep a copy wherever the harness runs; canonical = Darwin.
- **User-local personal data** (all in `~/.config/spela/`, NOT the public repo):
  `taste_profile.md` (the recommender SSoT — meta-rules + genre map + "watched &
  loved" anchors), `rt_watchlist_2026-07-13.md` (RT watchlist import → slice-3 seed),
  `tvtime_watchlist_2026-07-13.md` (TV Time progress screenshots), `tvtime_export/`
  (the near-empty GDPR zip). The watched-ledger lives in `state.json`.
- **TV Time GDPR export is a dead end for history** (verified 2026-07-13: 3
  byte-identical exports, account/settings/IP only, zero watch data). Screenshots
  are the record.
- **Completed-history capture is incomplete.** The TV Time screenshots are the
  watchlist + upcoming tabs (progress on tracked shows), NOT a completed-shows list,
  so finished-and-loved shows aren't fully captured — they're accumulated into the
  user-local `taste_profile.md` "watched & loved" list (title list stays user-local,
  not this public repo). If TV Time has a watched/profile view, screenshot it before
  the 2026-07-15 shutdown for a full pass; else keep accumulating manually.
- Future spela iOS app (if built) is for playing/remote, not re-tracking — the tracker
  lives in the shared backend + the existing web-remote PWA.
