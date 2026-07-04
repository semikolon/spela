# spela barrage findings — 2026-07-05 (overnight unattended)

Systematic latency/stability/resilience testing across 20 titles spanning
blockbuster → ultra-obscure, driven by `/tmp/barrage.sh` against an isolated
test instance. Goal (Fredrik): "make them stream as perfectly as they can."

## Test rig (representative, isolated from production)

A SECOND spela instance on Darwin, port 7891, casting to "Fredriks TV" MUTED
(volume 0 — audio leaks into the bedroom via the HDMI switch). Launched with:
`SPELA_STATE_DIR=~/.spela-test SPELA_MEDIA_DIR=~/media-test SPELA_EPHEMERAL_DHT=1
TMDB_API_KEY=<from prod unit> spela server --host 192.168.4.1 --port 7891`.

- **`SPELA_EPHEMERAL_DHT=1`** (added this session): DHT stays ON but skips the
  shared `~/.cache/com.rqbit.dht/dht.json`, so librqbit binds an ephemeral
  OS-assigned UDP port → real peer discovery, zero collision with production's
  persistent DHT. The earlier `SPELA_DISABLE_DHT` (trackers-only) starved peer
  discovery (859-seed torrent saw 1 peer at t=3s) and inflated every latency
  measurement — do NOT use it for representative testing.
- Kill the test instance ONLY by port (`fuser -k 7891/tcp`), NEVER `pkill -f`
  (production's ExecStart is the identical `spela server --host 192.168.4.1`).

## CRITICAL bug fixed — cross-instance ffmpeg kill (caused Fredrik's looping Silo)

`do_cleanup` ran `pkill -9 -f "ffmpeg.*transcoded_hls"`. Both instances' ffmpeg
command lines contain "transcoded_hls" (prod `media/transcoded_hls`, test
`media-test/transcoded_hls`), so **every test-instance cleanup SIGKILLed
production's live Silo transcode** → it died at 3 segments → the Chromecast
looped those ~18s in an endless recast loop (cast_health_monitor auto-recast
#8, #9, #10…). Fixed by scoping the pattern to `<media_dir>/transcoded_hls`
(the two paths are mutually non-substring). Commit `b91e804`.

## Regression fixed — alass "Preparing video…" 16s stall

The 2026-07-04 densest-track subtitle fix extracted EVERY embedded text track
(6 French tracks on the Silo MULTI release) to find the densest → ~16s of
"Preparing video…". Now orders non-forced-first + stops at the first dense
(≥20-cue) track — typically 1 extraction. Still avoids the sparse forced-track
mis-align trap (forced-French 8-cue → wrong -0.8s; full-French 663-cue →
correct -5:55). Commit `b91e804`.

## Latency root causes (torrent cold-start 33–58s even with good peers)

Two compounding costs, both DHT-independent (confirmed with ephemeral DHT +
9–16 peers):

1. **Pre-buffer waits for 20 primary HLS segments before casting**
   (`server.rs` `min_segments = 20` for chromecast) ≈ 126s of content ≈ ~40s
   of the block time. This is the dominant cost. Local-Bypass plays use the
   race-ahead gate (12s lead / 2 segments, MIN_SPEED 1.5) and start in ~7–13s;
   the torrent path is far more conservative. The "42 segments" in the log =
   `count_hls_segments` counting both adaptive variants (seg_0 + seg_1); the
   real gate is primary segment index 20.

2. **Head-of-stream probe (10s) + retry cascade** — the byte-0 probe abandons
   result#1 if the swarm hasn't delivered byte 0 in 10s (slow unchoke: peers
   connected, but no data). `handle_play` then retries result#2 from scratch
   (fresh unchoke). Often helps (a faster alt release), but each retry costs
   ~10s + the next torrent's startup; genuinely-slow swarms (e.g. Sicario 720p)
   cascade past 75s.

**Inherent floor**: a fresh torrent can't stream before byte-0 arrives
(unchoke ~10–15s on a moderate swarm). The ~6s target is realistic only for
Local Bypass (already-downloaded). Well-seeded fast swarms (Drive: 7.5s) prove
the pipeline is fast when the swarm cooperates.

## Fixes in flight / proposed (ranked)

- **[TESTING] Reduce chromecast pre-buffer 20 → 10 segments** — the single
  biggest torrent-latency win (~20s). 60s buffer + Chromecast buffer + 15s
  monitor BUFFERING tolerance remains safe. Validate via re-barrage: confirm
  all still PLAYED with no new cast_health_monitor BUFFERING/STALLED events.
- **[DESIGN] Unify torrent cast-timing under `race_ahead_safe`** — cast at a
  small lead once segments are produced ≥1.5× realtime (download sustaining
  above playback), else wait. Matches the proven local-file model; the right
  long-term shape but a bigger change (its own careful arc).
- **[DESIGN] Race the top-2 ranked results in parallel**, cast whichever
  produces a streamable transcode first, stop the loser. Eliminates the
  sequential retry-churn AND picks the genuinely-fastest swarm (the resilience
  win). Larger architectural change — parallel torrents + parallel pre-buffer
  + pick-winner + cleanup.

## Per-torrent baseline (min_segments=20) — filled after barrage completes

<!-- RESULTS_TABLE -->
