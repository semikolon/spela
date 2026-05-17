# Requirements: spela web remote

## Overview
- **Type**: Feature (new client surface over the existing spela HTTP API)
- **Problem**: Controlling spela today means a terminal or asking Claude. There is no human-usable surface to search, pick, play, and (especially) **seek/rewind** from a phone — the entire May-16/17 movie-night thrash was caused by this gap.
- **Pain point**: Friction/annoyance bordering on blocked — every play/seek/rewind is a CLI round-trip.
- **Success unlocks**: A standalone, daily-use control surface; also the **UX prototype that de-risks the native iOS app** in `PHONE_APP_PROJECT.md` (answers its Tier-1 "remote vs watch-on-phone" with real usage instead of speculation).
- **This is the `PHONE_APP_PROJECT.md` "A. spela-served web remote" path.**

## Pre-articulated constraints (decided, NOT open — recorded, not asked)
- **Dark theme.** Snappy + **non-glitchy** is a first-class requirement (NFR-1), not polish.
- **Served BY spela**, same-origin (`/remote`), mirroring the existing `include_str!` + `serve_static_with_range` static pattern → zero CORS/Host friction.
- **Mobile-first.** LAN at home; away = the existing official **WireGuard** app (WG is NOT baked into the web app).
- **No auth** — LAN/WG is the security boundary (matches the current unauthenticated API + `PHONE_APP_PROJECT.md`).
- **Single active stream (v1)** — matches spela's single `current` backend. Simultaneous phone+TV is a flagged backend refactor → out of v1 (see Out of Scope).
- **Reuse existing endpoints**; `SearchResult`/`/status` already carry `poster_url` (the `PHONE_APP_PROJECT.md` "posters missing" gap is **stale/already-filled** — zero backend work for art).
- Single self-contained dark HTML/JS asset, **no build step / no npm toolchain** (greenfield frontend; inherently snappy; dev effort is not a selection factor per global CLAUDE.md).

## Resolved non-obvious decisions (the only things asked)
- **D1 — v1 scope: BOTH cast-control AND in-browser phone playback are first-class.** (Constrained by single-stream backend — see US-5 + Risks.)
- **D2 — "My Library" browse of the curated BOHR collection is in v1.** Requires a new `serve-library` `/library/list` endpoint + spela aggregation.
- **D3 — Search UX is hybrid:** tap → plays the ranker top pick; a "more sources" affordance reveals ranked releases for when the auto-pick is wrong (the tonight HEVC-vs-BOHR failure mode).

---

## User Stories

### US-1 — Search & play (hybrid) — *Primary*
**As** the household viewer, **I want** to search a title and have it just play, **so that** I never touch a terminal.
- AC-1.1: A search field returns spela `/search` results as a poster grid (`poster_url`, title, year).
- AC-1.2: A **Movies / TV toggle** maps to `&movie=1` (the tonight "Sleepover auto-detected as TV" miss → explicit toggle).
- AC-1.3: Tapping a result POSTs `/play` with the **ranker top pick** and the current play target; playback starts with no further taps.
- AC-1.4: Each result card has a subtle **"more sources"** disclosure → the full ranked list (quality / codec / seeds / size / source); tapping an alternative plays that specific release.
- **When** a search yields zero results, **the system shall** show an empty state with the Movies/TV toggle hint (not an error).
- **If** `/play` returns an error (dead seeds, fail-fast), **then the system shall** surface spela's error message and offer "try another source" (the more-sources list).

### US-2 — Now-playing transport incl. one-tap rewind — *Primary*
**As** the viewer, **I want** play/pause/seek/scrub/rewind/next/prev without the CLI, **so that** the tonight pain never recurs.
- AC-2.1: While a stream is active, a now-playing bar shows poster, title, position/duration (from `/status`).
- AC-2.2: Controls map 1:1 to existing endpoints: pause→`/pause`, resume→`/resume`, next→`/next`, prev→`/prev`, stop→`/stop`.
- AC-2.3: A **⏮ "restart"** one-tap button issues `/seek 0` (tonight's exact recurring ask → first-class).
- AC-2.4: A scrubber issues `/seek <absolute-secs>` (v3.4.3 absolute-episode-position semantics).
- **While** the stream is live (still transcoding, no `#EXT-X-ENDLIST`), **the system shall** clip the scrubber's seekable max to the transcoded frontier (derived from `/status`) and visually mark the un-seekable region; **once** the set is complete, **the system shall** allow full scrub.
- **While** in cold-start (~20–60 s, no media session yet), **the system shall** show an explicit "starting…" progress state — never a blank/blue ambiguous state or a premature error (the tonight "blue-icon confusion" is an explicit embarrassment criterion).

### US-3 — My Library (BOHR) browse — *Primary (D2)*
**As** the viewer, **I want** to browse my curated BOHR collection as posters and tap-play, **so that** my own library is first-class, not only torrent search.
- AC-3.1: A "My Library" view renders the aggregated curated collection (BOHR Demeter films + TV) as a poster grid.
- AC-3.2: Posters are best-effort TMDB-enriched from the parsed title; missing art falls back to a clean titled tile (never a broken image).
- AC-3.3: Tapping a library item plays it via the existing remote-origin **bridge** path (do_play → serve-library match) to the current target.
- **If** the library origin is unreachable, **the system shall** show "library offline" (distinct from "empty") and still allow Search.

### US-4 — Cast target / device selection
**As** the viewer, **I want** to choose where it plays, **so that** I can send to the TV or watch on this phone.
- AC-4.1: A target picker lists video-capable Chromecasts from `/targets` (audio-only devices filtered out) **plus** a "This phone" option.
- AC-4.2: The selected target persists for the session and is sent with every `/play`.

### US-5 — In-browser phone playback (D1)
**As** the viewer, **I want** to watch on the phone itself (bed/travel), **so that** it's not only a TV remote.
- AC-5.1: With target = "This phone", the app plays `/hls/master.m3u8` in an in-page player.
- AC-5.2: Native HLS via `<video>` where supported (iOS Safari — the iPhone-heavy household per `PHONE_APP_PROJECT.md`); **hls.js** lazy-loaded as the Chromium fallback.
- AC-5.3: Phone-direct transport (play/pause/seek) drives the `<video>` element; the same live-vs-VOD seek-window rule (AC-2.4) applies.
- **When** the user switches target between "This phone" and a TV, **the system shall** make explicit that this **moves the single active stream** (it does not run both) — see Risks R-1.

---

## Non-Functional Requirements

### NFR-1 — Snappy & non-glitchy (first-class)
- Zero cumulative layout shift: fixed poster aspect-ratio cells, reserved space, skeleton/shimmer placeholders.
- Optimistic controls: every button reacts in <50 ms locally, reconciles on the next `/status`.
- Debounced search (~300 ms); lazy `loading="lazy"` poster images; CSS `content-visibility`/`contain` for 60 fps grid scroll.
- Instant dark paint (inline critical CSS, no theme flash); no heavy framework; single asset, no build.
- `/status` polled only while a stream is active and the tab is foregrounded (Page Visibility API) — pause polling otherwise (battery + snappiness).

### NFR-2 — Resilience / error philosophy
- **Fail gracefully + informatively.** "spela unreachable" → clear retry; never silently wedge.
- Cold-start is a state, not an error (don't time out the UI before spela's own gates).

### Success criteria (gut check)
- The author uses it for an entire movie night with **zero terminal/Claude round-trips**, including the rewind that triggered this whole project.

### Embarrassment criteria (must NOT ship with)
- Janky scroll / layout shift / theme flash.
- Ambiguous "is it loading or broken?" state during cold-start (the tonight blue-icon problem).
- A wrong-source auto-play with no recovery path (must have the more-sources escape hatch).

---

## Out of Scope (v1)
- **Simultaneous phone + TV playback** (single-stream backend; multi-stream = separate backend refactor — R-1).
- Offline download; multi-user state / per-user watchlists.
- Full Netflix genre-browse / recommendations / hero carousel (v1 IA = Search + My Library + History/Continue-watching).
- The native iOS app (separate `PHONE_APP_PROJECT.md` track — this web app de-risks it).
- Auth; the $5 Custom Cast Receiver (TV-side, orthogonal); SSE/WebSocket status (v1 = lightweight polling).
- A dedicated phone 720p transcode profile (LAN/WG handles the existing 1080p H.264; revisit only if real-world bandwidth proves it).

---

## Risks & Assumptions
- **R-1 (load-bearing):** user chose "both equal priority" but the backend is **single active stream**. v1 mitigates via an explicit target switch that *moves* the one stream; true concurrency is documented out-of-scope. If concurrency becomes required, it's a `state.rs`/`server.rs` refactor (own spec).
- **R-2:** My Library poster art depends on parsed-title→TMDB lookup quality; mitigated by clean titled fallback tiles.
- **R-3:** phone-direct relies on mobile-Safari native HLS / hls.js for Chromium; Safari-first is acceptable (household is iPhone-heavy).
- **R-4:** first cold My-Library list / bridge play can be slow until the v3.6.3 **self-warm** half is deployed (tracked in `TODO.md`); v1 must show the cold-start state gracefully (NFR-2), not error.
- **Assumption:** LAN/WG is a sufficient security boundary (no auth) — consistent with the current API and `PHONE_APP_PROJECT.md`.

## Open Questions
- None blocking v1. Deferred: dedicated phone transcode profile (R-3/Out-of-scope), SSE upgrade, native-app go/no-go (informed by v1 usage).
