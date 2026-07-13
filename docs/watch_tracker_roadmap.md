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
   **UI polish (2026-07-13):** rows show a half-faded poster background + the
   release year after the title, via `GET /title-meta` (TMDB `poster_and_year`,
   disk-cached in `watchlist_meta.json`, lazy per-row). Search box + Movies/TV
   selector share one line; section headers use `.twhead` (small all-caps).
   **Search detail card (2026-07-13):** "✓ Mark watched" button (→ watched-ledger,
   which now also excludes the title from To-Watch), plus a "🍅 Rotten Tomatoes →"
   read-more link (RT *search* URL — robust for movies AND series, no key; a
   hand-built `/m/<slug>` 404s on disambiguated titles per the research).

6. **Critic blurb (LLM-written, cached) — FUTURE, deferred 2026-07-13.** RT-as-a-
   source is out: MDBList gives scores + a link but **no consensus prose**; RT's
   written consensus is **movies-only** (OMDb `tomatoConsensus`) and **TV consensus
   has no clean/legal structured source** (scrape-only, against Fandango ToS — see
   `docs/rt_score_fetch_research_2026-07-13.md`). The only path that covers movies
   AND series is an **LLM-written critic-sentiment blurb** (the recommender's Claude
   harness): synthesize 1-2 lines from a spread of critics/reviews, **cache to disk**
   (like `/title-meta`), labeled as a summary (not a quote). Deferred until the
   recommender harness work. **Prefer NOT to scrape RT** — not forbidden, just
   (a) brittle: Cloudflare-fronted Next.js that re-skins → silent breakage
   (decay-horizon), (b) ToS-tainted (negligible enforcement risk for personal
   cached use, but a real term), and above all (c) MOOT: the LLM-from-a-spread
   approach covers movies AND TV without depending on RT's page, so a dedicated
   RT scraper buys nothing and is strictly more fragile. Scraping is only the sole
   option if a VERBATIM RT consensus quote is wanted — then (a)/(b) are the
   tradeoff. Fredrik: *"Maybe an LLM-written (and cached) blurb from a bunch of
   critics/reviews, in the future."*
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

5. **Auto-track any house Chromecast (non-spela casts) — ✅ SHIPPED (2026-07-13).**
   `spawn_chromecast_tracker` (60s tokio task) + `probe_device_media` (any-app,
   read-only) + `pending_watched`/`dismissed_watched` + `GET /pending-watched`
   + `POST /pending-watched/resolve` + the web-remote confirm prompt. Detector
   is live; validates in production as the household uses the TVs (observe-first
   INFO logging learns per-app metadata shapes). Design below unchanged.
   Poll each house Chromecast's media status; match to TMDB; auto-mark watched —
   so shows watched on Netflix/YouTube/Disney+ (not via spela) still land in the
   tracker with zero manual effort. **Cadence: 60s** (media state changes slowly;
   Darwin is the router so keep the poll light; configurable via
   `auto_track_poll_secs`). **Config gate:** `auto_track_chromecasts` (default
   **true** — Fredrik approved). **Self-referential-safety:** READ-ONLY polling
   (`receiver.get_status` + `media.get_status`), NO control actions, only writes
   the local watched-ledger — a standing tokio-independent std-thread loop with no
   external side effects (bg-task-guard's concern is mutating side effects; this
   has none).

   **Design (implementation spec):**
   - A dedicated `std::thread` spawned at server start (rust_cast is BLOCKING —
     must NOT run on the axum runtime; mirrors `connect_with_retry`'s blocking
     model). 60s loop; gated off if `auto_track_chromecasts=false`.
   - For each configured video Chromecast (reuse `Chromecast Devices` list —
     Fredriks TV + Vardagsrum; hardcoded fallback IPs in cast.rs), connect +
     `receiver.get_status()` → `applications[]`.
   - **Read media status for ANY app_id, not just CC1AD845.** The media namespace
     `urn:x-cast:com.google.cast.media` is standard, so `device.media.get_status(
     transport_id, None)` works for any app implementing it. **Per-app metadata
     quality varies** (roadmap caveat, now concrete): the Default Media Receiver
     + Disney/YouTube expose title/series/season/episode + current_time/duration
     well; **Netflix uses a PRIVATE namespace and often reports little/nothing via
     the standard media namespace** → partial coverage, honest. So: OBSERVE-FIRST
     — log every observed `{app_id, title, series, S/E, player_state, ct/dur}` at
     INFO for the first while so we learn real per-app shapes before hardening the
     matcher (Observe-don't-predict; the shapes can't be predicted from docs).
   - **Skip spela's OWN casts** (already tracked via `save_position_smart`): skip
     when `content_id` contains the stream host / `/hls/` (spela's HLS URL), so we
     never double-count.
   - **Match** the observed title (+ year if present) → `tmdb_auto_detect` /
     `tmdb_search` → imdb_id + season/episode; TV metadata already carries S/E.
   - **STAGE, don't auto-mark — a Chromecast observation is HEURISTIC, not proof
     it was FREDRIK (2026-07-13, load-bearing correction).** Housemates watch the
     house TVs too (anchor: they're mid-Euphoria — episodes Fredrik's already
     seen). So a near-complete session (`current_time/duration ≥ 0.96`) is
     appended to a **pending-confirmation queue**, NOT the watched-ledger. On
     Fredrik's next web-UI load, the remote surfaces the pending items — "Watched
     on a TV recently? [title] — Yes, I watched it / No (someone else)". Only a
     **Yes** calls `AppState::mark_watched` (into HIS ledger). `Observable user
     intent dominates inferred/heuristic state` — his confirmation is the
     authority; the poll is only the detector. **spela's OWN casts are the
     exception** — those ARE provably his (he started them via spela) and stay
     auto-marked via `save_position_smart`; the confirm-flow is ONLY for
     externally-detected (non-spela) Chromecast sessions.
   - **Pending queue model:** `AppState.pending_watched: Vec<PendingWatch>`
     ({key, title, series/S/E, device, first_seen, last_seen}) in state.json.
     Dedup by key. **Don't re-queue** a key that's already in the watched-ledger
     (he's seen it) OR in a `dismissed_watched` set (he said "No" — so a housemate
     binge doesn't re-nag every poll; each NEW episode is a new key, so genuinely
     new episodes still surface). Endpoints: `GET /pending-watched` (list),
     `POST /pending-watched/resolve` ({key, watched:bool} → yes: mark_watched +
     dequeue; no: dequeue + add to `dismissed_watched`).
   - Conservative near-complete gate (0.96) means a brief tune-in never stages.
     No live-Chromecast test needed to ship the detector — it self-observes in
     production; the confirm-prompt validates every stage against Fredrik's real
     intent, so a mis-detection costs one "No" tap, never a wrong ledger entry.

   **rust_cast API (verified in cast.rs `get_info`, lines 442-496):**
   `device.receiver.get_status()?.applications[]` → each `{app_id, transport_id}`;
   `device.connection.connect(transport_id)?` then
   `device.media.get_status(transport_id, None)?.entries.first()` →
   `{player_state, current_time, media:{duration, content_id, metadata}}`;
   `extract_metadata_title(&Metadata)` (cast.rs:560) pulls the title;
   `Metadata::TvShow{series_title, episode_title, season, episode}` carries S/E.

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
