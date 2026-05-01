# Incident Report — Wilderpeople movie-night cast failure (5-bug debug arc)

**Date discovered**: 2026-04-30 evening (Wilderpeople movie night)
**Date resolved**: 2026-05-01 afternoon
**Symptom**: every `spela play` to either Chromecast (Fredriks TV / Vardagsrum) ended with `idle_reason=ERROR` on the receiver and `process_dead` on spela's side. Movie was watched on Netflix as a fallback; spela was unusable.
**Resolution**: 5 commits across `master` (`2c598f4`, `27b07b3`, `af3fc41`, `3dff940`, `dcbaed7`) plus one revert (`40809e5`). Final state: cast loads in 2-4s; 287 unit tests pass; receiver returns `BUFFERING` not `LOAD_FAILED`.

---

## Summary

5 distinct bugs, layered on top of each other. The first 3 were latent; the 4th was introduced cosmetically by an Apr 30 fix attempt; the 5th was the actual user-visible blocker. Removing earlier-layer bugs revealed deeper ones — the diagnosis required peeling them off in order. The deepest bug (CORS) was a regression introduced by the Apr 30 H2 security audit.

| # | Layer | Symptom (when isolated) | Commit |
|---|-------|-------------------------|--------|
| 1 | spela bound to LAN IP only; ffmpeg's hardcoded `127.0.0.1` URL got `Connection refused` | HLS pre-buffer timeout (60s) with 0 segments | `2c598f4` |
| 2 | `cast_health_monitor` killed cold-starting streams at stream_age=20s, before Chromecast could finish DMR LOAD ack | `chromecast media session DEAD` after 3 IDLE polls | `27b07b3` |
| 3 | librqbit `start()` returns before storage is ready; ffmpeg's first GET arrives during `initializing` window → 404 → ffmpeg fail-fast | ffmpeg log: `Server returned 404 Not Found` on `/torrent/N/stream/0` | `af3fc41` |
| 4 | NVENC produced Main profile for low-res input but master claimed High@4.0 (cosmetic — receiver doesn't enforce CODECS strictly, but worth fixing for correctness) | Master CODECS string lied — pre-emptive fix | `3dff940` |
| 5 | **Apr 30 H2 commit (`14671d4`) tightened CORS from `allow_origin(Any)` to specific-origin LAN allowlist. Dropped `Access-Control-Allow-Origin: *` from m3u8 responses when Origin wasn't in allowlist. Cast Receiver runs on `https://www.gstatic.com/cast/...` and uses MSE-based HLS playback — MSE strictly enforces CORS. Without `Allow-Origin` echoed back, MSE rejects the manifest** | Receiver returns `LOAD_FAILED` → `idle_reason=ERROR` instantly | `dcbaed7` |

---

## Diagnostic chain (in order)

### Phase 1 — surface symptom

User reported: cast says "streaming" in `spela --human status`, but bedroom TV shows blue cast icon and never plays. Same Wilderpeople file plays fine on Netflix on the same TV → receiver hardware is fine.

### Phase 2 — peel off bug #1 (loopback)

Read `~/.spela/ffmpeg.log`:
```
[tcp @ 0x...] Connection to tcp://127.0.0.1:7890 failed: Connection refused
[in#0 @ 0x...] Error opening input: Connection refused
Error opening input file http://127.0.0.1:7890/torrent/3/stream/0.
```

Spela's systemd unit binds via `--host 192.168.4.1` (LAN-only, intentional — Darwin is the router and `0.0.0.0` would expose 7890 on WAN). But ffmpeg's URL builder hardcodes `127.0.0.1`. Loopback wasn't bound. **Fix**: `compute_bind_addresses()` returns dual-bind when `--host` is non-loopback non-wildcard. 7 regression tests pin the WAN-not-exposed invariant.

### Phase 3 — peel off bug #2 (cold-start kill)

After fix #1, ffmpeg connects fine and produces segments at 20.7x speed. But `cast_health_monitor` still kills the stream at `stream_age=20s`:
```
state transition: "<init>" → IDLE idle_reason=None time=0s media_session=None
... player_state=IDLE (1/3 consecutive idle polls before cleanup)
... player_state=IDLE (2/3 consecutive idle polls before cleanup)
... player_state=IDLE (3/3 consecutive idle polls before cleanup)
PRE-CLEANUP for 'Hunt for the Wilderpeople' — stream_age=20s ... media_session=None
```

The monitor was treating cold-start IDLE the same as mid-stream death. Apr 18 incident in CLAUDE.md established BUFFERING-startup-window protection (`MIN_STREAM_AGE_FOR_RECAST_SECS = 60`); IDLE got no equivalent protection. **Fix**: pure helper `is_idle_in_cold_start_window(media_session_id, prev_player_state, stream_age, window)` — three conjuncts (no session yet AND no prior PLAYING/BUFFERING AND within window). 6 regression tests including a Wilderpeople-repro pinning test.

### Phase 4 — peel off bug #3 (librqbit init race)

After fix #2, monitor gives 60s grace. But ffmpeg fails for a different reason on fresh-start:
```
[hls @ 0x...] HTTP error 404 Not Found
Error opening input file http://127.0.0.1:7890/torrent/1/stream/0.
```

Spela log:
```
torrent{id=0}:initialize_and_start: Doing initial checksum validation, this might take a while...
spela::transcode: ffmpeg HLS args: [...]   (ffmpeg starts here)
spela::torrent_stream: handle.stream(1, 0) failed: with_storage_and_file: invalid state: initializing
torrent{id=0}:initialize_and_start: Initial check results: have 1.3Gi, needed 0
```

`librqbit::Session::add_torrent()` returns when the torrent is added to the session; the storage backing isn't ready until initial-checksum-validation completes (1-3s for cached files). spela's `do_play` kicks ffmpeg off immediately after `start()` returns. Race window. **Fix**: in `serve_torrent_stream`, retry `handle.stream(file_idx)` every 250ms for up to 30s while the error string contains `"initializing"`. Beyond 30s, return `503` (so ffmpeg's `-reconnect` IS triggered). Other errors still 404.

### Phase 5 — false trail #4 (CODECS-truth)

After fix #3, ffmpeg produces segments cleanly. But cast still fails. The master playlist hardcoded `CODECS="avc1.640028"` (H.264 High@4.0) but ffmpeg encoded as Main profile for the 720x304 XviD source. Hypothesis: MSE rejects on CODECS mismatch. Forced `-profile:v high -level:v 4.0` so NVENC produces what the master claims.

**This didn't fix the cast.** ffmpeg now produced High@4.0 (verified via ffprobe), but the receiver still returned ERROR. Conclusion: CODECS mismatch wasn't the actual blocker — the receiver doesn't enforce CODECS strictly enough to fail on this. Fix is kept anyway (correctness improvement) but didn't address the user-visible symptom.

### Phase 6 — bisect to find the actual regression

Discovered Apr 25 docs in CLAUDE.md showed spela was working then. Bisected against the Apr 30 v3.3.0 commits.

- Pre-v3.3.0 (`17ef4c0`): wouldn't build on current Darwin (webtorrent CLI uninstalled).
- `7445530` (H1 SSRF only): **cast PLAYS in 2s.** ✅
- `14671d4` (H2 Host-header + tightened CORS): **cast FAILS with ERROR.** ❌

Bisect confirmed across 3 trials. H2 is the regression.

### Phase 7 — pinpoint within H2

Compared HTTP response headers via `curl -sI -H "Origin: http://example.com"`:
- **`7445530`**: response includes `access-control-allow-origin: *`
- **`14671d4`**: no `Access-Control-Allow-Origin` header at all

H2 changed `CorsLayer::allow_origin(Any)` → `CorsLayer::allow_origin(specific_LAN_origins)`. tower_http's `CorsLayer` only echoes back `Access-Control-Allow-Origin` when the request's `Origin` header matches the allowlist. Cast Receiver runs on `https://www.gstatic.com/cast/...` — its `Origin` header is `https://www.gstatic.com`, NOT a LAN URL — so no `Allow-Origin` came back. Cast Receiver uses MSE-based HLS playback, MSE strictly enforces CORS on cross-origin manifest fetches → MSE rejects → DMR returns `LOAD_FAILED`.

### Phase 8 — fix #5

Reverted CORS to `allow_origin(Any)`. Verified post-deploy: response now includes `access-control-allow-origin: *`; pychromecast cast → `BUFFERING` in 4s; user confirmed audible BipBop and Wilderpeople playback on Fredriks TV.

---

## Diagnostic techniques worth keeping

### `idle_reason=ERROR` is opaque — capture raw MEDIA messages via pychromecast

`rust_cast` and pychromecast both surface `idle_reason=ERROR` with no further detail when DMR rejects a LOAD. The actual `LOAD_FAILED` event is a separate Cast SDK message on the `urn:x-cast:com.google.cast.media` namespace. Subscribe to ALL incoming messages on that namespace to see it:

```python
from pychromecast.controllers import BaseController

class MediaListener(BaseController):
    def __init__(self):
        super().__init__('urn:x-cast:com.google.cast.media')
        self.messages = []
    def receive_message(self, message, data):
        self.messages.append(data)
        print(json.dumps(data, indent=2)[:1500])
        return True

target.register_handler(MediaListener())
mc.play_media(URL, content_type, stream_type='BUFFERED')
mc.block_until_active(timeout=10)
time.sleep(12)
```

Output during this incident:
```json
{"type": "MEDIA_STATUS", "status": [{"playerState": "IDLE",
  "extendedStatus": {"playerState": "LOADING", "media": {...}}}]}
{"type": "LOAD_FAILED", "requestId": 5, "itemId": 1}
{"type": "MEDIA_STATUS", "status": [{"playerState": "IDLE",
  "idleReason": "ERROR"}]}
```

`LOAD_FAILED` is the smoking gun. `idle_reason=ERROR` is just the surfaced consequence.

### Apple BipBop reference HLS as a known-good baseline

`https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_4x3/bipbop_4x3_variant.m3u8` is a public HLS reference that works on every Cast Receiver. Casting it to a suspect device confirms whether the receiver itself is OK. If BipBop plays and your URL doesn't, the issue is in your content / server / response headers — NOT the receiver, the network, or rust_cast/pychromecast.

### HTTP header diff between known-good and broken

Once isolated to "our content vs. Apple's content," `curl -sI -H "Origin: https://www.gstatic.com"` against both URLs and diff the response headers. The CORS regression in this incident was visible from a single header diff (`access-control-allow-origin: *` present vs missing).

---

## Universal principles (worth promoting to global CLAUDE.md)

1. **CORS `allow_origin(Any)` is REQUIRED for any HTTP server serving HLS to a Cast Receiver / MSE client.** Tightening CORS to a specific-origin allowlist drops `Access-Control-Allow-Origin: *` for non-matching origins, which MSE strictly rejects. Use Host-header allowlist as the actual DNS-rebinding defense — that's the primary protection per H2's own design comment, and it stays effective regardless of CORS.
2. **`idle_reason=ERROR` from Chromecast is opaque — capture raw MEDIA messages to see `LOAD_FAILED`.** Subscribe to `urn:x-cast:com.google.cast.media` via a pychromecast `BaseController` to surface the actual Cast SDK error class.
3. **Apple BipBop reference HLS plays on every receiver. Use it as the known-good baseline for receiver isolation.**
4. **librqbit `start()` returns before storage is ready** — code that consumes the storage immediately after `start()` must retry on `invalid state: initializing` (transient) for several seconds.

---

## Why the bisect took multiple trials

Initial bisect at `7445530` succeeded but I couldn't immediately reproduce that success. Cause: receiver state. Cast LOAD failures left DMR in a wedged state where even valid LOADs intermittently failed. Reproducing the bisect required:

1. Pychromecast `target.quit_app()` between tests to fully reset the receiver app
2. spela `kill-workers` + `stop` between tests to reset spela-side state
3. Multi-second sleeps between rebuild and test to let systemd settle

Once those gates were honored, the bisect was 100% deterministic across 3 trials.

---

## Generic lessons (linkable from global CLAUDE.md)

- **A failure with multiple compounding causes is debugged by peeling — fix the surface bug, observe the next layer, repeat.** Not by trying to identify the "real" bug from observation alone. Each fix here was real and necessary; none were wasted.
- **Security tightening that adds Vary headers or restricts CORS origin lists can silently break legitimate non-browser HTTP clients (or browser-based clients with unexpected origins like Cast Receiver).** Always check that the response surface matches what the legitimate consumer expects.
- **Bisect with `git checkout <sha>` + rebuild is the most reliable diagnosis when symptoms are device-side and opaque.** Days of guessing at stream content / format / codecs are worth less than 30 minutes of disciplined `git bisect` against a known-working commit.
- **The hard-won-lesson "Validate UI-symptom fixes against historical user observation" (Apr 29, in global CLAUDE.md) was honored here**: user said "spela worked months ago" → looked for a regression rather than rebuilding receiver-compatibility from scratch.

---

## Cross-references

- `~/Projects/spela/CLAUDE.md` § Hard-Won Lessons — terse versions of the 5 bugs
- `~/.claude/CLAUDE.md` § Code Changes — universal CORS-MSE principle
- Bisect commits: `7445530` (works) ↔ `14671d4` (breaks) — H2 alone is the boundary
- Final fix commit: `dcbaed7`
