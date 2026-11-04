# Design: spela web remote

## Tech Stack (Derived from the spela repo — not asked)
- **Backend**: Rust, axum 0.8, tower-http 0.6 (cors only — no ServeDir; the repo serves static via `include_str!` + a hand-rolled `serve_static_with_range` handler + an explicit route, see `server.rs:4258` `RECEIVER_HTML`/`serve_static_with_range`/`/cast-receiver.html`).
- **Frontend**: greenfield (only `static/cast-receiver.html` exists; no `package.json`, no toolchain). → **single self-contained `static/remote.html`** (HTML + inline CSS + vanilla ES modules). No build, no npm — inherently snappy, mirrors the existing pattern, one artifact `include_str!`'d into the binary.
- **One allowed runtime dep, lazy-loaded**: `hls.js` (CDN-pinned or vendored into `static/`) for Chromium HLS in phone-direct mode only; iOS Safari uses native `<video>` HLS (no lib).
- **API**: existing JSON-over-HTTP on `:7890`, same-origin from `/remote` (no CORS/Host concerns; the Host-allowlist + `allow_origin(Any)` already permit it).

## Architecture Overview

```
 Phone browser (/remote, dark SPA, same-origin)
   │  fetch() JSON
   ▼
 spela server (the server :7890, axum)
   ├── EXISTING: /search /play /pause /resume /seek /status /targets /history /queue
   ├── EXISTING: /hls/master.m3u8        ← phone-direct <video> source (US-5)
   ├── NEW: GET /remote                  ← serve static SPA (include_str! + serve_static_with_range)
   └── NEW: GET /library                 ← aggregate curated library (US-3)
                │ fan-out
                ▼
        remote_origins → serve-library  GET /library/list   (NEW on serve-library; sibling of /library/match)
        (+ any local config.library_dirs, same enumeration)
```

Single-active-stream invariant preserved: the SPA holds a `target` (a Chromecast name **or** `"__phone__"`); every `/play` carries it; switching target re-points the one `current` stream (US-5 / R-1). The SPA never assumes two concurrent streams.

## File Structure
```
~/Projects/spela/
├── static/
│   └── remote.html              # NEW — the entire SPA (HTML+CSS+JS, no build)
│   └── hls.min.js               # NEW — vendored hls.js (Chromium phone-direct fallback)
├── src/
│   ├── server.rs                # +route GET /remote, +handler; +GET /library aggregator;
│   │                            #  remote-origin library-list fan-out (mirror the /library/match
│   │                            #  liveness-ping + generous-timeout pattern from v3.6.3)
│   └── library_origin.rs        # +route GET /library/list (enumerate roots → entries+meta)
└── .claude/specs/web-remote/    # this spec
```

## Data Models

### Existing (consumed as-is — confirmed in repo)
`SearchResult` (`search.rs:19`): `id, title, tmdb_id, imdb_id, poster_url: Option<String>, quality, seeds, size, source, info_hash, file_index`.
`/status` `current` (`server.rs`/`state.rs`): `title, poster_url, duration: Option<f64>, ss_offset, pid, prepared_hls, url, status` + top-level `status/running/ffmpeg_alive`.

### NEW — `LibraryEntry` (returned by `serve-library GET /library/list`, aggregated by spela `GET /library`)
```rust
struct LibraryEntry {
    title: String,          // cleaned display title (parsed from folder/file name)
    year: Option<u32>,      // parsed
    raw_name: String,       // exact folder/file name (what do_play title-matches against)
    size_bytes: u64,
    container: String,      // "mkv" | "mp4" | …
    poster_url: Option<String>, // best-effort: reuse spela's TMDB client to look up parsed title
}
```
`GET /library` response: `{ "library": [LibraryEntry...], "origin_status": "ok|offline" }`. Posters enriched server-side (reuse the existing TMDB search used by the ranker); enrichment is best-effort and cached in-memory per origin scan.

## API Design

| Method | Path | New? | Purpose | Notes |
|---|---|---|---|---|
| GET | `/remote` | NEW | Serve the SPA | `include_str!("../static/remote.html")` via `serve_static_with_range`; mirror `/cast-receiver.html` |
| GET | `/library` | NEW (spela) | Aggregated curated library for the My-Library grid | Fans out to `config.remote_origins` `/library/list` + local `config.library_dirs`; **reuse the v3.6.3 liveness-ping (2 s, no-FS) + generous (25 s) timeout pattern** so a cold the library disk HDD doesn't 0-list it; merge + TMDB-enrich |
| GET | `/library/list` | NEW (serve-library) | Enumerate this host's library roots | Sibling of `/library/match`; walks the same configured roots (incl. the `the film archive…/` + `…/TV` roots); returns `[LibraryEntry]`; same security posture (no path leakage beyond root names) |
| GET | `/search?q=&movie=` | exists | Hybrid search | `&movie=1` from the Movies/TV toggle (the "Sleepover→TV" miss) |
| POST | `/play` | exists | Play (top pick or chosen source) | Body carries result id + `cast` target (Chromecast name) **or** the phone path |
| POST | `/seek` | exists | Scrub / ⏮ restart | Absolute episode secs (v3.4.3); `0` = restart (US-2 ⏮) |
| GET | `/status` `/targets` `/history` `/pause` `/resume` `/next` `/prev` `/stop` `/queue` | exists | Now-playing, devices, transport | Polled (status) / fire-on-tap (transport) |

### Phone-direct (US-5)
Target = `"__phone__"`: SPA still POSTs `/play` (no `cast`) so spela builds the HLS as usual, then the SPA plays `http://<same-origin>/hls/master.m3u8` in `<video>` (Safari native) or hls.js (Chromium). Transport for phone-direct drives the media element; `/seek`-equivalent = element `currentTime` within the manifest's available window (same live-vs-VOD clip rule). Cast vs phone is mutually exclusive (single stream).

### Error contract
Surface spela's own JSON `error` strings verbatim in a toast + offer the more-sources list on play failure. Network failure → "spela unreachable, retry" banner. Cold-start (`status: streaming` but no media session / low `prepared_hls`) → persistent "starting…" with elapsed timer, no error until spela's own fail-fast fires.

## Snappy / non-glitchy tactics (NFR-1 → concrete)
- Single inline-CSS dark SPA; first paint is dark (no flash); fonts system-stack.
- Poster grid: fixed `aspect-ratio: 2/3` cells + skeleton shimmer; `loading="lazy"`, `decoding="async"`, `content-visibility:auto`.
- Optimistic transport: tap → instant local state change + spinner-less affordance → reconcile on next `/status` (≤2–3 s poll, only while active+foregrounded via Page Visibility API).
- Debounced search input (~300 ms); abortable in-flight fetch on new keystroke.
- No client router/framework; hash-based view switch (Search / Library / Now-Playing) with CSS-only transitions; zero dependency except lazy hls.js (Chromium phone-direct only).

## Trade-off Decisions
| Decision | Choice | Rationale |
|---|---|---|
| Framework | None (vanilla, single file) | Snappy/no-build mandate; mirrors repo's static pattern; greenfield |
| Status updates | Poll `/status` (foreground+active only) | SSE is a future nicety; polling is simplest, non-glitchy with optimistic UI |
| Concurrency | Single stream; target switch *moves* it | Backend reality (R-1); concurrency is a separate refactor, out of v1 |
| Library posters | Server-side best-effort TMDB enrich + clean fallback tile | Avoids broken images (embarrassment criterion); reuses existing TMDB code |
| Cold library/bridge | Reuse v3.6.3 liveness+generous-timeout for `/library` fan-out | Don't 0-list or hang on a spun-down the library disk; consistent with the just-built robustness |

## Security
No auth (LAN/WG boundary; matches current API + `PHONE_APP_PROJECT.md`). `/library/list` must not leak absolute paths usefully beyond what `/library/match` already does (return raw_name/meta, not full filesystem paths in the client payload; resolution stays server-side via the existing handle mechanism). Same-origin SPA → no new CORS surface.

## Performance
LAN: API calls are ms; HLS is the existing 1080p H.264. Phone-direct over WG: 1080p H.264 is fine on modern phones; a 720p profile is explicitly deferred (R-3). Cold the library disk spin-up handled by the v3.6.3 pattern reused in `/library`.
