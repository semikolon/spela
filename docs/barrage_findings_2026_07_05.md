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

## Per-torrent baseline (min_segments=20, ephemeral DHT, muted)

14/20 PLAYED. Block time = search + retries + download-unchoke + 20-seg pre-buffer.

| Title | r1 seeds | block | outcome |
|---|---|---|---|
| Drive 2011 | 100 | **10.7s** | PLAYED (fast swarm, cast at 7 seg) |
| Colour of Pomegranates 1969 | 66 | 27.7s | PLAYED |
| Whiplash 2014 | 98 | 28.9s | PLAYED |
| Prisoners 2013 | 100 | 30.8s | PLAYED |
| Oppenheimer 2023 | 73 | 33.2s | PLAYED |
| Parasite 2019 | 132 | 40.0s | PLAYED |
| The Matrix 1999 | 24 | 42.1s | PLAYED |
| Lighthouse 2019 | 100 | 45.0s | PLAYED |
| Come and See 1985 | 30 | 48.3s | PLAYED |
| Stalker 1979 | 14 | 53.4s | PLAYED |
| Wake in Fright 1971 | 5 | 56.5s | PLAYED |
| Satantango 1994 | 29 | 57.8s | PLAYED |
| Dune Part Two 2024 | 859 | 58.2s | PLAYED (retry churn: r1 probe-timeout) |
| Vampyr 1932 | 19 | 62.5s | PLAYED |
| Sicario 2015 | 78 | >75s | slow-unchoke cascade |
| Arrival 2016 | 24 | >75s | still transcoding at 75s (segs=5) |
| Naked 1993 | 38 | >75s | slow-unchoke cascade (seeds ≠ fast byte-0) |
| Werckmeister 2000 | 9 | >75s | slow swarm |
| The Cremator 1969 | 4 | >75s | near-dead swarm |
| Hundstage 2001 | 0 | >75s | genuinely dead (correctly ended idle) |

**Read**: latency, not crashes, is the story — even 5–30-seed obscure films
(Vampyr 1932, Satantango) stream, just slowly. The ">75s" rows didn't return in
the 75s poll window; most were still cascading retries or transcoding (Arrival
had 5 segments). Production stayed idle/healthy across the whole run (pkill
scoping fix confirmed — no cross-instance kill). Claimed seed count is a weak
predictor of byte-0 speed (Sicario 78, Naked 38 both slow; Drive 100 fast).

## Slow-fail on all-dead results (secondary)

A 0-seed title (Hundstage) cascades through all retry candidates for >75s before
ending idle. Correct outcome, slow path. A faster "all candidates dead" detector
(e.g. abandon after N consecutive 0-peer probes) would fail such titles in
~10–15s instead of >75s. Lower priority than the pre-buffer win.

## Improved run (min_segments=10) — VALIDATED, deployed to production

Re-barrage, same rig. **Median PLAYED block time ~44s → ~24s (~45% faster).**
15/20 PLAYED (Hundstage flipped to PLAYED). **0 STALLED/BUFFERING escalations**
across both the barrage AND a dedicated 3-min sustained play (Oppenheimer: the
transcode raced 4 → 124 segments in 180s ≈ 4× realtime, so at 10 segments the
buffer only GROWS during playback — huge safety margin). Production stayed
healthy throughout (pkill fix). Deployed to production `26b5ec2`.

| Title | seeds | baseline | improved | Δ |
|---|---|---|---|---|
| Drive 2011 | 100 | 10.7s | **8.5s** | −2 |
| Colour of Pomegranates 1969 | 66 | 27.7s | **15.6s** | −12 |
| Whiplash 2014 | 98 | 28.9s | **17.1s** | −12 |
| Parasite 2019 | 132 | 40.0s | **19.0s** | −21 |
| Oppenheimer 2023 | 73 | 33.2s | **20.0s** | −13 |
| Matrix 1999 | 24 | 42.1s | **23.2s** | −19 |
| Prisoners 2013 | 100 | 30.8s | **23.6s** | −7 |
| Stalker 1979 | 14 | 53.4s | **24.8s** | −29 |
| Come and See 1985 | 30 | 48.3s | **27.5s** | −21 |
| Lighthouse 2019 | 100 | 45.0s | **28.8s** | −16 |
| Dune Part Two 2024 | 859 | 58.2s | **32.4s** | −26 |
| Satantango 1994 | 29 | 57.8s | **34.7s** | −23 |
| Wake in Fright 1971 | 5 | 56.5s | **46.8s** | −10 |
| Vampyr 1932 | 19 | 62.5s | **51.0s** | −12 |
| Hundstage 2001 | 0 | >75s FAIL | **69.1s PLAYED** | ✓ |
| Sicario / Arrival / Cremator / Naked / Werckmeister | 4–78 | >75s | >75s | slow swarm (never enough segments; pre-buffer doesn't help) |

**Remaining slow-swarm cases** (Sicario 78, Naked 38 seeds but slow byte-0):
these produce few/no segments in 75s — the fix is the racing-sources /
smarter-retry design above, NOT the pre-buffer. Left for a dedicated arc.
