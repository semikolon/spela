# Tasks: spela web remote

> **STATUS (v3.7, shipped + deployed Darwin) — v1 FEATURE-COMPLETE:** **ALL TASKS DONE** — T-1..T-18 ✅ (T-4 TMDB poster enrichment shipped+deployed 2026-05-18). **T-16** real-device e2e = ongoing via the user's normal use (pause/resume bug found this way + fixed). Beyond spec scope: portless `spela.home` + the now-LIVE systemic-HTTPS successor (`https://spela.fredrikbranstrom.se`, spec `~/dotfiles/docs/lan_https_dns01_wildcard_spec_2026_05_18.md`). My-Library is **LIVE**: the Mac serve-library was a stale binary missing `/library/list` (NOT a "TCC follow-up" — that framing was a misattribution); rebuilt+restarted 2026-05-18 → 57 films + T-4 posters end-to-end. Only open follow-up: serve-library's TCC grant is code-signature-keyed so a Mac rebuild revokes it (re-Allow needed) — stable-codesign durability fix tracked at `TODO.md` "TCC durability hardening".

## Overview
- **Scope**: M (frontend is a single no-build asset; the real new backend work is `/library/list` + the `/library` aggregator).
- **External blockers**: none for v1. Cold-BOHR latency is mitigated by reusing the v3.6.3 liveness+timeout pattern; the v3.6.3 **self-warm** deploy (tracked in `TODO.md`) makes My-Library snappier but is NOT a blocker.
- **Cut-first if time-boxed**: US-5 phone-direct (D1) → ship cast-remote + Search + My-Library first; phone-direct second. Then My-Library before search "more sources" polish.
- **Speed vs correctness**: core-correct (search/play/seek/library reliable), edges iterative.

---

## Phase 1 — Backend: serve the SPA
- [ ] **T-1**: Add `static/remote.html` stub + `GET /remote` route + handler via `serve_static_with_range` (copy the `/cast-receiver.html` / `RECEIVER_HTML` pattern at `server.rs:~4258`). Verify `curl :7890/remote` 200s. *(unblocks all frontend)*

## Phase 2 — Backend: My Library endpoints (US-3, the only substantial new backend)
- [ ] **T-2**: `serve-library` `GET /library/list` — enumerate the configured roots (reuse the same root-walk as `/library/match`; include the `Demeter…/` films + `…/TV`), return `[LibraryEntry]` (title/year parsed from raw_name, size, container, raw_name). No absolute-path leakage. Depends: none (sibling of existing match).
- [ ] **T-3**: spela `GET /library` aggregator — fan out to `config.remote_origins` `/library/list` + local `config.library_dirs`; **reuse the v3.6.3 liveness-ping (2 s, no-FS) + generous (25 s) timeout** so a cold/absent origin yields `origin_status:"offline"` not a hang/empty; merge entries. Depends: T-2.
- [x] **T-4** ✅ (2026-05-18): `SearchEngine::movie_poster` (fail-soft, one `/search/movie`, reuses `tmdb_search`+`tmdb_poster_url`) + `handle_library` enriches per UNIQUE title with per-scan dedupe + bounded `JoinSet` concurrency. `None` on miss → titled fallback (AC-3.2). Verified live: 57 entries, 35 TMDB-resolved. Frontend already rendered `e.poster_url` — zero frontend change.

## Phase 3 — Frontend shell (NFR-1 foundation)
- [ ] **T-5**: `remote.html` dark SPA shell — inline critical CSS (instant dark, no flash), system font stack, hash-routed views (Search / Library / Now-Playing), zero framework. Depends: T-1.
- [ ] **T-6**: Reusable poster-grid component — fixed `aspect-ratio:2/3` cells, skeleton shimmer, `loading="lazy"`/`decoding="async"`, `content-visibility:auto`, broken-img → titled fallback tile. Depends: T-5.
- [ ] **T-7**: API client module — `fetch` wrappers, abortable in-flight, network-error → "spela unreachable, retry" banner (NFR-2). Depends: T-5.

## Phase 4 — Search (US-1) + Now-Playing (US-2)
- [ ] **T-8**: Search view — debounced (~300 ms) input, **Movies/TV toggle** (`&movie=1`), results into the poster grid. Depends: T-6, T-7. AC-1.1/1.2.
- [ ] **T-9**: Hybrid play (D3) — tap poster → `POST /play` (ranker top pick + current target); per-card **"more sources"** disclosure → ranked list (quality/codec/seeds/size/source) → tap = play that id. Play-error → toast spela's `error` + auto-open more-sources. Depends: T-8. AC-1.3/1.4.
- [ ] **T-10**: Target picker — `GET /targets`, filter audio-only, add **"This phone"**; persist for session; sent with every `/play`. Depends: T-7. US-4.
- [ ] **T-11**: Now-Playing bar — poll `/status` (only active + foregrounded via Page Visibility), render poster/title/position/duration; transport buttons → `/pause /resume /next /prev /stop`, **optimistic** then reconcile. Depends: T-7. AC-2.1/2.2.
- [ ] **T-12**: Scrubber + **⏮ restart** — slider → `/seek <absolute secs>` (v3.4.3); ⏮ → `/seek 0`; clip seekable max to transcoded frontier while live (derive from `/status`), full once complete; explicit persistent **"starting…"** cold-start state (no premature error / no blank). Depends: T-11. AC-2.3/2.4 + embarrassment criteria.

## Phase 5 — My Library view (US-3)
- [ ] **T-13**: "My Library" view — render `GET /library` as the poster grid; `origin_status:"offline"` → "library offline" state (≠ empty); tap → `/play` by title via the existing bridge path + current target. Depends: T-4, T-6, T-9, T-10. AC-3.1/3.2/3.3.

## Phase 6 — Phone-direct playback (US-5 / D1)
- [ ] **T-14**: Vendor `static/hls.min.js`; phone-direct player view — target `"__phone__"`: `POST /play` (no cast) then play `/hls/master.m3u8` in `<video>` (Safari native) / lazy hls.js (Chromium). Depends: T-1, T-10.
- [ ] **T-15**: Phone-direct transport — drive the media element (play/pause/seek-in-window); **explicit "moves the single stream" affordance** when switching phone↔TV (R-1). Depends: T-14, T-12. AC-5.1/5.2/5.3.

## Phase 7 — Verify & ship
- [ ] **T-16**: Local gates — `cargo fmt --check && cargo clippy -- -D warnings (new code) && cargo test`; manual e2e on a real phone (LAN): search→hybrid-play→TV; My-Library→bridge→TV; phone-direct watch; ⏮ restart; cold-start state; spela-down state.
- [ ] **T-17**: Commit (vX.Y) + deploy Darwin (rebuild + restart) per the established per-host pattern; smoke-test `/remote` from the phone.
- [ ] **T-18**: One-line pointer in `TODO.md` (project convention: TODO references specs); note phone 720p-profile + SSE + native-app-go/no-go as deferred follow-ups informed by real v1 usage.

---

## Verification Checklist
- [ ] All ACs (US-1..US-5) pass on a real phone over LAN.
- [ ] Gut check: a full movie night with **zero terminal/Claude round-trips**, including ⏮ restart.
- [ ] No embarrassment criteria: no layout shift / theme flash / jank; cold-start is an explicit state; wrong-source always has the more-sources escape.
- [ ] Single-stream invariant honored (target switch *moves* the one stream; no false concurrency).
- [ ] `cargo fmt`/clippy(new)/test green; new code clippy-clean.

## Notes
- **Conscious debt (documented):** single-stream (no phone+TV concurrency) — R-1; polling not SSE; 1080p-only (no phone profile) — R-3; library posters best-effort.
- Reuse, don't reinvent: TMDB client (enrichment), `serve_static_with_range` (SPA serve), the v3.6.3 liveness+timeout (library fan-out), `SearchResult.poster_url` (already present).
