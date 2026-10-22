# spela as watch-tracker + recommender — roadmap

**Why:** TV Time shuts down 2026-07-15, and every polished tracker (Hobi, Simkl, …)
is ads-or-subscription. spela already plays everything, knows what you play, and
stores per-episode resume state — so it can *be* the tracker: free, ad-free, no
subscription, yours, and integrated with search/play + the recommender. This
replaces the external-tracker migration entirely.

**Data placement (public-repo rule):** the spela repo is PUBLIC. All personal
watch/taste data is user-local under `~/.config/spela/` (like `config.toml`):
`taste_profile.md`, `tvtime_watchlist_2026-07-13.md`, and `watchlist.json`.
The watched ledger and resume positions live in `state.json`, not a separate
`watched.json`. Only non-personal feature design lives in the repo.

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
   **Seen-detection is EPISODE-COUNTED, not name-matched (2026-08-26).** The
   To-Watch hide-check keyed on the show NAME alone, so ONE watched episode erased
   a whole series from the list — four minutes of a pilot hid a 96% show for weeks.
   `AppState::hidden_watched_shows` now needs 3 DISTINCT episodes for a series;
   films, explicit whole-show marks and migrated `seed` baselines still hide on
   one. The threshold is deliberately forgiving: a to-watch list holds CANDIDATES,
   so a half-abandoned show costs one row while a barely-sampled one costs the
   whole recommendation. The ledger also supports removal through `POST /watched-remove {title}`,
   with a rec-row Undo toast and a separate Rewatch shelf for finding and correcting
   seen titles. The former collapsed Watched section on To-Watch has moved to Rewatch;
   keeping seen titles discoverable is what makes mistaken marks recoverable.
   Detail: CLAUDE.md Hard-Won "To-Watch hides a SERIES only past 3 distinct watched episodes".

2b. **Followed-shows "New Episodes" tracker — ✅ SHIPPED (2026-07-28→29, v3.19).**
   **Progress model reworked v3.20 (2026-08-02): unified into the watch-ledger as the
   single source of truth — `following.json` is now a MEMBERSHIP set only, unfollow
   no longer loses progress, re-following resumes it. Full design + tasks:
   `.claude/specs/unify-watch-progress/`. The description below reflects the v3.20 model.**
   The TVTime replacement, deterministic (no LLM). `following.rs` +
   `~/.config/spela/following.json` (user-local: {title, tmdb_id, imdb_id} — a
   MEMBERSHIP set; progress DERIVES from the watch-ledger via
   `AppState::derive_watched_through`, not stored here). `GET /following` joins it
   with TVmaze-corrected schedules and cached source evidence (`tv_status_sources`),
   retaining TMDB as schedule fallback; `episode_air` resolves the next-unwatched
   episode's name/date through the same schedule source. See
   [release availability](EPISODE_AVAILABILITY.md) for cadence, errors and fallback limits.
   The response gives, per show:
   new-episode count (season-aware), next-unwatched (SxxExx + title + aired-date),
   next upcoming air-date. UI: a "New Episodes" landing row + subtle header count
   badge + the same section atop To-Watch (poster-bg rows, ep title, air dates;
   tap → search the next ep). Progress advances via the watch-ledger (v3.20 — ONE
   write path; the follow view derives `watched_through`): the **✓ button** (`POST
   /following/mark {tmdb_id,season,episode}`, next ep → `mark_watched_auto`); **spela
   completion** (`save_position_smart` → `mark_watched`, the Chromecast/HLS path spela
   position-tracks); and **VLC completion** — the Mac-side
   `spela-vlc-bridge` launchd agent, since Open-in-VLC serves raw byte-ranges
   (spela is blind to VLC's playhead). VLC's HTTP control interface exposes the
   playhead (`vlcrc extraintf=http` + password on the Mac → `127.0.0.1:8080/requests/
   status.json`); the agent gets the show identity from spela `/status` and POSTs
   `/position {imdb_id,title,seconds,duration}` EVERY poll → `save_position_smart`
   updates the resume HWM (mid-episode cross-device resume) AND at ≥`HWM_CLEAR_FRACTION`
   clears the resume + marks watched (the follow view then DERIVES the advance from the
   ledger — v3.20). **Gotcha
   (2026-07-29):** spela's `/status` reports `vlc_active` only ~30s after VLC's last
   byte-range fetch, so a fully-buffered episode goes quiet and the identity vanishes
   there → the watcher must hold identity STICKY (tied to the media's length) and post
   to the end, else completion never fires (Silo S03E03 froze at 70% before this fix).
   VLC is NOT opaque — the byte-progress proxy was rejected (MKV tail-reads/seeks) in
   favour of the true playhead. Setup step to reproduce on a fresh Mac: enable VLC's
   web interface in vlcrc + install the agent plist (`com.fredrikbranstrom.spela-vlc-bridge`).

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
   **The interactive assistant is the recommendation harness** (Fredrik: "the
   LLM is YOU until I say otherwise"). Spela provides the store, endpoints, metadata
   lookup and ledger; Codex or Claude supplies the reasoning. No model API or autonomous
   curation runs inside Spela. A future automated harness should use the same contract;
   it is an idea, not a deployed background job. Taste preferences and discovery criteria
   live in the user-local `taste_profile.md`; they are not server-enforced score gates.
   Distinguish discovery criteria from the bounded RT-plus-taste **display sort**
   described in [To-Watch presentation](#to-watch-presentation).

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

7. **Unified home queue + recommender — implemented.** The backend owns progress,
   followed-show availability and seen filtering; the interactive assistant curates
   suggestions. The UI gives continuing and newly available episodes priority, while
   discovery lives on To Watch. Two pieces, arsenal-and-harness:
   - **Recommender arsenal** (`recommendations.rs` + `~/.config/spela/recommendations.json`,
     user-local): `GET /recommendations` (serve, excludes already-seen via
     `AppState::has_seen`, case-insensitive) + `POST /recommendations` (the harness writes
     the full ranked list; replace semantics; capped 100). Each `RecEntry` carries a one-line
     **`why`** — the rationale IS the value. The harness is the interactive assistant
     (Codex or Claude): it reads
     `taste_profile.md` + `/watched` + `/watchlist` + air-dates, ranks, and writes picks.
     There is no autonomous recommendation refresh in the server.
   - **Continue store** (`AppState.in_progress`, keyed like `resume_positions`): upserted in
     `save_position_smart`'s HWM branch (title/show/S·E/poster from the live `CurrentStream`;
     duration + position for the %), cleared on completion / `reset_position`. Needed because
     `resume_positions` holds only seconds — no duration/title/poster to render a Continue row.
   - **Unified home** `GET /home` (NOT `/queue` — that route is the play-FIFO auto-fire list):
     returns `continue`, `new_episodes`, `upcoming`, `total_new`, and `recommended`.
     Recommendations are de-duplicated against Continue, New Episodes and seen titles
     in this response. The endpoint's recommendation field is retained even though
     the Home UI does not currently render it.
   - **Home UI** (`static/remote.html`, `loadHome`): Continue → New Episodes →
     Coming Soon. Home is the default landing; Continue tap re-searches and resumes.
     The backend still returns `recommended`, but the current Home renderer does not
     display it; curated recommendations live on To Watch. Do not infer visible sections
     from the `/home` payload alone.
   - **Poster + live-score enrichment** (`enrichRec`): To-Watch eagerly queues
     `/title-meta` through the shared bounded loader; numeric live RT overrides the
     stored curated score. The rationale remains stored prose. See
     [To-Watch presentation](#to-watch-presentation) for current behavior and limits.
   - **Continue/New-Eps dedup + mark-seen + non-linear counting — ✅ SHIPPED (2026-08-03).**
     (i) A Continue episode that IS a followed show's next-unwatched folds into its New-Eps row as
     "▸ NN% into SxxExx" (parses SE from the title, since a VLC-tracked play leaves the struct
     fields None) instead of a duplicate. (ii) A ✓ "already seen" button on each Recommended row
     (→ `/watched-add`, dropped from recs next load, optimistic remove). (iii) **Per-season seen
     overlay** (`FollowedShow.seen_seasons` + `POST /following/seen-seasons` + `unseen_after` /
     `next_unwatched_skipping`): a ☰ expander on each New-Eps row reveals S1..SN toggle chips;
     marking whole seasons seen subtracts them from the count, so a middle-seen gap is
     representable (the single caught-up HWM couldn't). Verified: Fargo 47→6 new (marked S2-5
     seen, only unseen S01E05-10 remain), chips reflect state, count refreshes on panel close.
   - **Harness re-run 2026-08-26** (`generated_by: claude-taste-2026-08-26`, 90 picks):
     eight new entries lead — Deadloch / Mr Inbetween / Bad Monkey for the
     Poker-Face-shaped hole (Poker Face is cancelled at Peacock; Rian Johnson is
     shopping it with Peter Dinklage, recasting Charlie every two seasons by design),
     Scavengers Reign promoted from rank 71, plus Furious / Dept. Q / The Chestnut Man
     / Rose of Nevada from a 2026 sweep. The prior 82 are preserved beneath, since the
     pool is curated and bulk-pruning it is Fredrik's call, not the harness's.
   - **NEXT (refinements, not blockers)**: (b) an optional
     `/recommend`-style refresh affordance (button that re-triggers the harness), or a mood
     input ("how much do you have tonight?") the profile's meta-rules call for; (d) a Phase-2
     model-swappable autonomous harness (same POST surface).
   Tests: `has_seen` case-insensitive exclusion + in-progress write/clear (`state.rs`).

8. **Rewatch shelf — ✅ SHIPPED (2026-08-27).** `#/rewatch` lists the watch-ledger as a
   browsable shelf: episodes collapse to their show, rows split **Films / Series**, films
   read as `2h 16m` and series as episode/season counts (runtime is the number you scan
   when choosing what fits tonight). Media type comes from `GET /watched`, which attaches
   it from the title-meta DISK cache so the split happens on FIRST paint rather than
   shuffling rows as they scroll in. **The type-evidence order matters**: cached type >
   an `SxxExx` in the ledger title > ask the server (`media_type=auto`) — the ABSENCE of
   `SxxExx` proves nothing, since a show marked at SHOW level looks exactly like a film,
   which is how Station Eleven resolved to an unrelated 2013 movie. Detail: project
   CLAUDE.md § Rewatch tab + § `GET /watched`.

## Notes
- **`has_seen` answers "is this a discovery?", NOT "would he watch it tonight?" (2026-08-27).**
  The ledger backfill marked both the Matrix and Terminator franchises seen, and Fredrik is
  openly rewatch-willing on the later entries. Excluding them from RECOMMENDATIONS is right;
  the **`#/rewatch` view SHIPPED 2026-08-27** reads the same ledger from the other side and
  deliberately does not inherit that exclusion; any further themed-night surface should
  follow it rather than the recommender. Rewatch-openness is
  a separate axis, recorded per-title in `taste_profile.md`.
- **The recommender's quality gate is 82% (Fredrik, 2026-08-26), lowered from ~88%.**
  Devs (82%) was loved while sitting below the old bar, which showed the gate was
  filtering well-matched titles rather than unknown ones. Lane-fit outranks the number;
  the score is still always stated. Full rule + bands: `taste_profile.md` (user-local).

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
  not this public repo). TV Time shut down 2026-07-15, so that capture route is closed.
  **The cost is measurable, not theoretical**: on 2026-08-26 the gap produced FIVE
  recommendations of already-loved titles in a single session. **Newly-identified
  import path**: Netflix's PER-PROFILE viewing activity (`netflix.com/viewingactivity`,
  CSV) plus Amazon's data request — use the per-profile page, since the family account's
  GDPR export pulls every profile. Blocked on a Bitwarden re-login for the credentials.
  Tracked with the full flow in `TODO.md` § Watch tracker.
- **The ledger is the SSoT for seen-status; `taste_profile.md` is NOT (2026-08-26).**
  `has_seen` reads the ledger, and `GET /recommendations` filters every pick through it
  at read time — so a title named "loved" in the profile but never written to the ledger
  stays fully re-recommendable. The profile's lists are taste ANCHORS (each carries a
  *reason* the ledger cannot hold), which is why the duplication is deliberate rather
  than drift. Writing one without the other is the failure mode, and it is silent.
  The rule + the one-line `POST /watched-add` that closes the chain are documented at
  the foot of `taste_profile.md` (user-local, Darwin).
- Future spela iOS app (if built) is for playing/remote, not re-tracking — the tracker
  lives in the shared backend + the existing web-remote PWA.

## To-Watch presentation

Recommended for you is the curated suggestion set (`recommendations.json`), with
reasons and curator rank; Series/Movies are the broader saved pool (`watchlist.json`).
Neither is an alias for the other. Overlap is intentional; keep the sections separate.

Within each To-Watch section, RT critic score is the main ordering input. Curated
rank supplies a bounded 1–5-point taste boost (top rank gets five), also applied to
matching saved titles; absent curation contributes zero. This is a weak proxy for
personal fit, not a newly computed taste model. Unknown RT goes below known scores;
updated metadata scores update the ordering. Recommendation cards use the same
two-column desktop breakpoint as saved rows (641px), single-column below it.

To-Watch starts metadata/artwork loading at page load, including below-fold rows.
The shared request queue permits four concurrent metadata lookups, reuses in-flight
and five-minute cached requests, and retries failed lookups on a subsequent load.
Matching recommendation identities enrich saved entries to improve request reuse.
Recommendation requests enter the queue before the lower shelves; following metadata
does not block the rest of the page. Row backdrops request w780 rather than w1280.
Rewatch retains viewport-based enrichment; do not generalize eager unbounded
loading to the whole app. This prefetch downloads metadata/artwork only, never media.

### Curation and maintenance boundaries

`why` is stored assistant-written prose, not generated or revised by the page. Metadata
refresh updates artwork and scores, but cannot repair a weak rationale or stale claims
inside it. Explain the work's concrete appeal; comparisons must name a different work
and be grounded in the taste profile. Do not imply the user has seen a suggestion merely
because it appears among taste anchors. Prefer durable descriptions over claims such as
“highest-rated on your list,” which become false as scores or the list change.

Keep `title`, `media_type`, year and known TMDB/IMDb IDs together: artwork and score
lookup use these identities, and ambiguous titles without them can resolve incorrectly.
Check duplicate identities within a curated list; overlap *between* recommendations
and saved shelves is intentional. Personal picks and taste data stay in the server's
user-local config directory, never the public repository.

`GET /recommendations` is a seen-filtered view, not a lossless export of the store.
`POST /recommendations` replaces the entire list (maximum 100) and stamps generation
provenance. For a single-copy repair, preserve the full stored set and its provenance,
back up first, and atomically replace the user-local file; do not round-trip the filtered
GET response as though it were the complete list. Read back through the endpoint to
verify. A data-only edit is visible on the next page load without a service restart.

### Scope and limits

The RT-plus-taste ordering is a **To-Watch presentation rule**, including its New
Episodes section. It neither rewrites curator ranks nor changes Home's section priority,
backend recommendation order, or torrent-source ranking. The taste adjustment is relative
to the ranks in the currently returned recommendation set; it is not a stable personal
rating. Live numeric RT replaces the stored score for display and sorting; absent live
scores retain any stored score. With neither available, the title sorts below rated ones.

The four-request limit covers metadata lookups; subsequent image downloads use normal
browser scheduling and caching. The five-minute/256-entry cache is in this page's memory,
not persistent storage or a server cache change. Prefetch removes the scroll trigger and
w780 reduces image size, but cold upstream lookups can still take time. No measured
end-to-end latency improvement is promised. Recommendation metadata is queued before
saved shelves; `/following` no longer blocks those shelves, but initial rendering still
awaits `/watchlist` and `/recommendations`. These are deliberate limits, not claims that
every image is ready immediately when the page opens.
