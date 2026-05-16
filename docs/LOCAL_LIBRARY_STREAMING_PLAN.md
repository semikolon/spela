# Local Library Streaming — Design & Implementation Plan

**Status**: PLANNED (2026-05-16) · **Target**: spela v3.6.0 · **Author**: design session 2026-05-16

> Goal: when a requested title already exists as a finished media file somewhere
> on the LAN (an archive drive attached to the Mac, any drive on the Mac, any
> drive on the spela-server host), stream **that file** instead of
> re-downloading the torrent. Re-torrenting a file we already own is strictly
> unnecessary, slow, and wasteful.

---

## 1. Problem statement

spela already has a **Local Bypass** mechanism (`server.rs:find_local_bypass_match`,
`do_play` ~857): before starting a torrent it scans the server's `media_dir`
(`~/media`) for a healthy on-disk file matching the requested title/year/quality
and, on a hit, feeds that `file://` path into the *same* transcode→HLS→cast
pipeline a torrent would use (`pid = 0`, `is_local = true`).

Two limitations:

1. **Single root.** It only scans `media_dir` (the torrent download/cache dir).
   It does not scan curated external libraries.
2. **Single machine.** It only scans the **filesystem of the host running the
   spela server**. Libraries physically attached to a *different* LAN machine
   are invisible to it.

This plan removes both limitations.

## 2. The non-obvious crux (read this before anything else)

**The spela server runs on the server host (`<SERVER_HOST>`). A USB/archive
drive attached to a *different* machine (the workstation, `<WORKSTATION_HOST>`)
is NOT on the server's filesystem.** `std::fs::read_dir("<workstation-only
path>")` on the server returns "no such directory" → silent fallthrough to
torrent. The naive "just add the path to a config list" approach **silently
fails** for any library that lives on a machine other than the server.

Whoever can `open()` the bytes must be the one to serve them. This forces a
distributed design: a small file-origin component on each machine that hosts a
library, which the server queries and streams *from*.

## 3. Architecture decision

### Considered

| Option | Verdict | Reason |
|---|---|---|
| **Cross-mount the workstation drive onto the server (NFS/SMB/SSHFS)** | ❌ Rejected | macOS network-FS clients are documented data-corruption / hang hazards in this fleet (see operator's global storage-strategy notes). Streaming ffmpeg through a corrupting mount is the worst possible consumer. Non-negotiable no. |
| **Pre-copy (rsync-over-SSH) workstation→server, then play** | ❌ Rejected as primary | Reliable transport, but a multi-minute copy before playback defeats the "smooth / strictly unnecessary" goal. It is just cross-machine Smooth Mode. Retained only as a possible explicit fallback, not the path. |
| **Server-hosted libraries only** (scan extra roots on the server host) | ◐ Partial | ~5-line change, but only covers content physically on/attached-to the server. Does not stream a workstation-attached archive while it stays on the workstation. |
| **File-origin on each library host + server transcode-bridge** | ✅ **CHOSEN** | The only option that covers libraries on *any* LAN machine without the corrupting mount. Reuses ~100% of the existing pipeline. New surface is small and well-bounded. |

### Chosen: file-origin + server transcode-bridge

```
                 ┌────────────────────── server host (spela server) ──────────────────────┐
 spela play 1 ─▶ │ do_play                                                                 │
                 │   1. find_local_bypass_match(media_dir ∪ library_dirs)  ── hit ─▶ file:// │
                 │   2. else: GET <origin>/library/match?title=…           ── hit ─▶ http:// │
                 │   3. else: torrent (unchanged)                                           │
                 │            │                                                              │
                 │            ▼  server_url (file:// OR http://workstation/library/stream)   │
                 │   detect_codecs(server_url)  ── ffprobe (works on file:// and http://)    │
                 │   transcode_hls(server_url, …)  ── NVENC, canonical H.264 profile         │
                 │            │                                                              │
                 │            ▼  /hls/master.m3u8  ── Chromecast LOAD points HERE (server)   │
                 └────────────┼──────────────────────────────────────────────────────────────┘
                              │ (range reads of raw file, on demand, over LAN HTTP)
                 ┌────────────▼──── workstation host (spela serve-library) ─────────────────┐
                 │ GET /library/match   → reuse find_local_bypass_match over Mac roots      │
                 │ GET /library/stream  → Range-capable raw-file serve (security-bounded)   │
                 └─────────────────────────────────────────────────────────────────────────┘
```

The server stays the **sole transcode + cast authority**. A library host is a
**dumb matcher + range file server**. The Chromecast always fetches the HLS
stream from the server, exactly as today.

## 4. Critical constraint: Chromecast route is ALWAYS the bridge

`server.rs:1130-1131`:

```rust
let need_video_tc = if target == "chromecast" {
    true            // ← unconditional, even for already-H.264 sources
} else { … };
```

This is an **incident-anchored decision (May 12, 2026)**: CrKey 1.56 is
"materially happier" with a single canonical profile (H.264 High@4.0, 30 fps,
fixed 6 s GOP, AAC stereo). Merely HLS-wrapping a "compatible" source still
leaves too many source-dependent variables (50 fps levels, odd GOP cadence on
`-c copy`, undetected HEVC on partial probes).

**Consequence for this design**: the "direct-serve an H.264 mp4 straight from
the workstation to the Chromecast, zero transcode" optimization I originally
sketched is **contrary to a hard-won decision** and is **explicitly OUT of
scope for v1** (see §13 Deferred). For Chromecast, the route is *uniformly* the
server-NVENC bridge. This both respects Chesterton's Fence on `server.rs:1130`
and *simplifies* the implementation — there is no codec→route fork for
Chromecast; the only decision is the **source URL** (`file://` Darwin-local vs
`http://` workstation-origin), which `detect_codecs`/`transcode_hls` already
handle identically.

## 5. `is_local` semantics (subtle, load-bearing)

Today `is_local` ≈ "matched a local file, no torrent worker". It gates:
`should_wait_for_complete_hls_before_cast` (Reliability Mode), skips the
torrent download-progress gate, skips Smooth Mode.

For a workstation-origin source the file is **complete and immediately
fully-available** (it is a finished file, served over Range HTTP) — there is no
download race. Therefore **`is_local` must be `true` for workstation-origin
plays too**. Its real meaning is **"complete non-torrent source"**, not
"file:// path on this disk".

**Carve-out**: code that needs an actual local *path* (not just "complete
source") must stay `file://`-gated. The known case is embedded-subtitle
extraction at `server.rs:1004` (`server_url.starts_with("file://")`). For
workstation-origin plays we skip embedded-sub extraction and fall back to
OpenSubtitles (same as the torrent path). Darwin-local library files keep
embedded extraction. This carve-out is enumerated as a test.

## 6. Component A — config (`src/config.rs`)

Add to `Config` (all `#[serde(default)]` → existing configs unaffected,
no migration):

```rust
/// Extra roots on THIS host scanned for pre-existing media (in addition
/// to media_dir). Server-side. Searched after media_dir.
#[serde(default)]
pub library_dirs: Vec<String>,

/// Base URLs of remote spela `serve-library` origins to query when no
/// local match is found. Server-side. e.g. ["http://<WORKSTATION_LAN_IP>:7891"].
#[serde(default)]
pub remote_origins: Vec<String>,

/// Port the `spela serve-library` mode listens on (library-host side).
#[serde(default = "default_library_port")]
pub library_serve_port: u16,   // default 7891
```

`config.toml` is user-local and gitignored → real paths/IPs live there, never
in the repo. Example (NOT committed; documented here with placeholders):

```toml
# server host
library_dirs   = ["/mnt/hdd/library"]
remote_origins = ["http://<WORKSTATION_LAN_IP>:7891"]

# workstation host (runs: spela serve-library)
library_dirs       = ["$ARCHIVE_ROOT", "$OTHER_LIBRARY_ROOT"]
library_serve_port = 7891
```

## 7. Component B — `spela serve-library` (new `src/library_origin.rs`)

Minimal axum server, same binary, new subcommand (one artifact — no second
codebase; Forged-steel). Binds to the LAN; reuses the server's Host-allowlist
middleware.

### Routes

- `GET /library/match?title=<t>&quality=<q>&year=<y>`
  → run **the existing `find_local_bypass_match`** over the host's
  `library_dirs`. On hit, register the resolved absolute path in a
  short-TTL (e.g. 120 s) in-memory handle map and return
  `{ "handle": "<opaque>", "size": <bytes>, "container": "mkv|mp4" }`.
  On miss: `404`.

- `GET /library/stream?h=<handle>` → Range-capable raw file serve of the
  path the handle resolves to. **No raw path is ever accepted from the
  client** (eliminates path traversal by construction). Defense in depth on
  top of the handle:
  - resolved path is `canonicalize()`d and asserted to be a prefix-child of
    one configured `library_dirs` root (post-canonicalization, so `..` and
    symlink escapes are caught);
  - symlinks refused (reuse `local_bypass_file_is_healthy` philosophy,
    `server.rs:3644`);
  - handle unknown/expired → `410 Gone`;
  - `Range` honored (ffmpeg seeks); `Accept-Ranges: bytes`, correct
    `Content-Length`, `206` semantics — mirror the rigor of
    `serve_torrent_stream` / `parse_range_header` in `torrent_stream.rs`.

### Why a handle, not `?path=`

A raw `?path=` is an SSRF/traversal sink (cf. the H1/H4 audit history:
`validate_magnet_uri`, `require_loopback_source`). The handle is an opaque,
server-issued, expiring token that maps only to paths the matcher itself
returned. The client cannot express an arbitrary path. This is the single
highest-risk surface in the whole feature and is treated accordingly.

## 8. Component C — `do_play` precedence chain (`src/server.rs` ~857)

Replace the current single-root bypass block with:

```text
if req.title.is_some():
    # 1. server-host local (existing behaviour, now multi-root)
    for root in [media_dir] + config.library_dirs:
        m = find_local_bypass_match(root, title, quality, expected_bytes, corrupt)
        if m: server_url = "file://"+m; is_local = true; break

    # 2. remote origins (new) — only if no local hit
    if not is_local:
        for origin in config.remote_origins:
            r = GET {origin}/library/match?title=&quality=&year=   # 2s timeout
            if r.200:
                server_url = "{origin}/library/stream?h={r.handle}"
                is_local = true                                    # complete source
                remote_origin_play = true                          # for §5 carve-outs
                break
    # 3. else: existing torrent path (unchanged)
```

Everything downstream is **unchanged**: `detect_codecs(&server_url)` ffprobes
the http URL fine; `transcode_hls(&server_url, …)` already accepts http input
(the deprecated `transcode()` doc explicitly documents HTTP progressive input);
Chromecast LOAD still points at the server's `/hls/master.m3u8`.

Carve-outs (§5): `local_source_for_subs` (line ~1004) stays `file://`-only.
**Implemented without a `remote_origin_play` flag** (revised from the
original sketch): the existing `server_url.starts_with("file://")` guard
*already is* the carve-out — an `http://` origin URL simply doesn't match,
so embedded-sub extraction is skipped and OpenSubtitles is used, which is
exactly the intended behaviour. Threading an unused bool purely "for future
greppability" would (a) violate the repo/global YAGNI directive and (b)
fail `clippy -D warnings` (unused variable) since no consumer exists. A
prominent code comment at the remote-origin block documents the carve-out
instead. Revisit only if a non-`file://` *local-path* consumer is added.

## 9. Security analysis (new attack surface = the library-host listener)

| Vector | Mitigation |
|---|---|
| Path traversal (`../../etc/...`) | No client path input at all — opaque handle only; + post-`canonicalize()` root-prefix assert |
| Symlink escape (link inside root → outside) | `symlink_metadata` refusal (reuse `server.rs:3644` logic) |
| SSRF (server tricked into fetching attacker URL) | `remote_origins` is operator-config allowlist; server only ever builds the URL from a configured origin + a handle it just received from that same origin |
| Unauth LAN access to arbitrary files | Only matcher-returned paths are reachable (handle map); Host-allowlist middleware reused; bind LAN-scoped |
| Handle guessing | Opaque random token, short TTL, single-origin issuance |
| Disk/2GB amplification | Range serve is read-only, bounded by file size; no transcode on the library host |

A focused mini-audit of `library_origin.rs` is a Phase-3 gate before the
component is enabled on the workstation (Security-audit-before-public-exposure).

## 10. Test plan (RED-GREEN, MANDATORY)

**Phase 1 (pure, fixture-based — extends the existing 12-test
`find_local_bypass_match` suite):**
- multi-root scan finds a match in a non-first root
- precedence: `media_dir` hit wins over a `library_dirs` hit for the same title
- empty/missing `library_dirs` root is skipped, not fatal

**Phase 2:**
- `library_origin`: `/library/match` hit returns handle+size; miss → 404
- `library_origin`: `/library/stream` honors `Range` (206, correct
  Content-Length, suffix/open/bounded — reuse `parse_range_header` cases)
- **security RED first**: `?h=` traversal/symlink-escape attempts → refused
  (write the failing test, see it red, then implement the guard)
- `do_play`: remote-origin selected only on local miss; `is_local=true` for
  remote-origin; `remote_origin_play` skips embedded-sub extraction
- codec routing unchanged for Chromecast (regression pin: chromecast still
  `need_video_tc=true` regardless of source codec/host)

**Phase 3 (e2e, manual, documented):**
- the DTS-HD x264 mkv archive copy (bridge + audio-transcode path) → bedroom TV
- a bare H.264 mp4 (still bridged for Chromecast per §4) → bedroom TV
- Built≠Verified: confirm actual cast playback, not just HTTP 200s.

## 11. Deployment (per-host, explicit operator approval — NOT auto-run)

1. **Server host**: `git pull && cargo build --release && sudo systemctl
   restart spela`. Config gains `library_dirs` + `remote_origins`.
2. **Workstation host**: new launchd agent
   `com.fredrikbranstrom.spela-library` running `spela serve-library`;
   `~/.config/spela/config.toml` gains `library_dirs` + `library_serve_port`;
   fix the missing `~/.local/bin/spela` symlink (observed drift 2026-05-16).
3. Both restarts are shared-infra changes → each needs explicit per-host
   operator approval at execution time (Fleet/Infrastructure rule).

## 12. Phasing + acceptance criteria

- **Phase 1 — config + multi-root local scan.** AC: server scans
  `media_dir ∪ library_dirs`; precedence + skip-missing tests green;
  `cargo fmt --check && cargo clippy -- -D warnings && cargo test` green.
  Immediately useful for server-attached libraries.
- **Phase 2 — `serve-library` + bridge.** AC: `/library/{match,stream}`
  implemented + security tests green; `do_play` remote precedence wired;
  gates green.
- **Phase 3 — audit + e2e + deploy.** AC: mini-audit clean; e2e cast verified
  on the real bedroom TV; deployed with per-host approval.

## 13. Parameter-level decisions (non-blocking; reversible)

- `.mkv` even with clean codecs is still bridged+canonicalized for Chromecast
  (forced by §4 anyway; no special case).
- Server-local match beats remote (no LAN hop, lower latency).
- Single binary + `serve-library` subcommand (not a separate crate).
- `/library/match` remote query timeout = 2 s (fail fast → torrent fallback).
- Handle TTL = 120 s (covers detect_codecs + transcode start; refreshed if a
  play re-resolves).

## 14. Deferred / explicitly out of scope

- **Direct workstation→Chromecast serve (zero transcode)** for already-canonical
  H.264. Contradicts the `server.rs:1130` canonical-profile decision (§4).
  Revisit ONLY with a registered Custom Receiver (Shaka, more tolerant) or
  after empirical profiling proves CrKey 1.56 stability on copy paths. Tracked
  here so the rejection rationale is not lost.
- Multi-origin parallel `/library/match` fan-out (sequential is fine for a 2-3
  origin fleet; revisit if origins grow).
- Library indexing/caching (the scan is cheap; revisit only if root counts or
  sizes make per-request `read_dir` slow).

## 15. Risks + rollback

- **Risk**: library-host listener is a new file-serving surface. **Mitigation**:
  handle-only API + canonicalize-prefix + symlink refusal + Phase-3 audit.
- **Risk**: `is_local` semantic broadening regresses a `file://`-assuming
  branch. **Mitigation**: grep all `is_local` / `starts_with("file://")` uses;
  `remote_origin_play` flag; regression tests.
- **Rollback**: feature is additive + config-gated. Empty `library_dirs` +
  empty `remote_origins` = byte-identical behaviour to today. Revert =
  drop config keys (server) + stop the launchd agent (workstation). No data,
  schema, or torrent-path change.
