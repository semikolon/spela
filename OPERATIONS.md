# spela Operations

## Worker Safety

`spela` delegates torrent serving to `webtorrent-cli`, which runs as a Node.js
process, and delegates incompatible media conversion to `ffmpeg`. Treat these
as owned workers with their own failure domain. They must not be allowed to
outlive playback indefinitely or consume the whole host.

This section was added after a private production incident in which abandoned
WebTorrent workers consumed most host memory and swap for more than a day. The
host then appeared to have unrelated SSH, DNS, and container problems because
the entire userspace was under severe resource pressure.

## Invariants

- Process cleanup and media cleanup are separate operations.
- Emergency worker cleanup must terminate WebTorrent/ffmpeg workers only; it
  must not delete media, mark files verified, or rewrite playback history.
- User-facing playback cleanup may delete temporary transcode files, but that
  must stay explicit and separate from worker-only cleanup.
- `.spela_done` completion markers require a known expected byte size and a
  physical byte check. Never derive completion from playback duration alone.
- Every WebTorrent worker should be traceable to current playback state.
- At server startup, stale workers that are not owned by active playback should
  be reconciled and terminated with `SIGTERM`.
- `SIGKILL` is an escalation path only after a grace period and should be
  logged.
- Resource limits should be enforced outside the application as well, using a
  systemd unit, slice, or transient scope where possible.
- Worker spawn paths must always be paired with cleanup paths even on failure.
  Any error-return between `start_webtorrent` and the post-playback reaper
  spawn is a leak unless it explicitly tears the worker pipeline down.
  Specifically: cast failure, transcode failure, or any panic in the early
  phase of `do_play` must call `do_cleanup(&state)` on the way out.
- Disk accounting must use allocated blocks, not logical length.
  webtorrent file selection (`-s <idx>`) creates sparse placeholder files
  whose `metadata.len()` reports the full multi-GB torrent size while only
  a few KB exist on disk. Summing `len()` will trip the media cap before
  any real download. Use `metadata.blocks() * 512` everywhere disk usage is
  measured (`disk.rs::dir_size`, `Local Bypass` health checks).
- Disk safety has two independent layers: spela's own media-dir cap
  (`MAX_MEDIA_MB`) protects spela from itself, and the host-filesystem
  free-space floor (`MIN_FS_FREE_MB`) protects the rest of the host from
  spela. Both checks must pass before a new download starts. The host floor
  is best-effort — `None` on `df` failure proceeds rather than blocks, so
  a parser regression can never make spela unusable on its own.
- `stream_host` must be a private LAN IP (e.g. `192.168.4.1`) when the play
  target is a Chromecast. Chromecast devices hardcode Google DNS
  (8.8.8.8 / 8.8.4.4) and ignore the LAN's recursive resolver, so a
  hostname like `darwin.home` makes the receiver fetch a name it can't
  resolve, the LOAD silently fails, and `player_state` stays IDLE forever
  while every other LAN client sees spela just fine. spela startup WARNs
  if `stream_host` looks like a hostname.
- Cast pipeline output for chromecast targets is HLS (`/hls/master.m3u8`),
  not the legacy chunked-transfer fragmented MP4 path
  (`/stream/transcode`). The HLS path uses MPEG-TS segments because
  rust_cast's high-level `Media` struct doesn't expose
  `hlsSegmentFormat`, which CAF Receiver requires for fmp4 segments.
  The cast URL is the SYNTHETIC master playlist (`/hls/master.m3u8`,
  generated on the fly with hardcoded `CODECS="avc1.640028,mp4a.40.2"` +
  `BANDWIDTH=6000000` + `RESOLUTION=1920x1080`), not the bare media
  playlist — older Chromecast firmware refuses to load a media playlist
  without explicit codec / bandwidth / resolution hints.
- HLS manifest must stay at HLS v3-v4. Avoid `-hls_playlist_type event`
  and `-hls_flags independent_segments` — both bump the manifest to HLS
  v6, which the older Shaka Player on CrKey 1.56 firmware can't parse.
  (`-hls_version` is NOT a valid ffmpeg HLS muxer option.)

## Defense-In-Depth Plan

1. Application ownership:
   Store worker PIDs, process group IDs where available, start time, title, and
   ownership token in local state. Reconcile that state when the server starts
   and before new playback begins.

2. Worker-only cleanup:
   Provide a command or endpoint equivalent to `spela kill-workers` that sends
   `SIGTERM` to WebTorrent/ffmpeg workers without touching files. Keep this
   separate from `spela stop`. Implemented first slice: `spela kill-workers`
   runs locally on the media host and terminates WebTorrent plus Spela-owned
   `transcoded_aac.mp4` ffmpeg workers without touching media files or playback
   history.

3. Service containment:
   Run WebTorrent/ffmpeg in a bounded cgroup or transient systemd scope. Node's
   `--max-old-space-size` is useful but not enough by itself because multiple
   workers can still exhaust the host. Implemented first slice:
   `systemd/spela-resource-limits.conf` is a host-safe drop-in with
   `MemoryHigh`, `MemoryMax`, `MemorySwapMax`, `TasksMax`, and `CPUQuota`.

4. Watchdog:
   Add an external host watchdog that checks memory pressure, swap pressure, and
   orphaned WebTorrent workers. It may alert early and may terminate only
   clearly orphaned workers under dangerous pressure. It must not delete media.

5. Startup reconciliation:
   On `spela server` startup, kill stale WebTorrent workers from previous
   sessions if they are not owned by the current state. This protects against
   abandoned SSH sessions and previous crashes. Implemented first slice:
   startup preserves the current state's live WebTorrent PID, clears dead
   current-stream state, and terminates other local WebTorrent workers.

6. Documentation and tests:
   Keep regression tests around worker-only cleanup and stale PID handling.
   Keep this operations note linked from `README.md`, `CLAUDE.md`, and
   `TODO.md`.

7. Local-bypass verification:
   Local bypass is useful, but it must not turn sparse or partial files into
   trusted media. Search-result size metadata should travel into playback state
   so cleanup can write `.spela_done` only after physical bytes match the
   expected size. If size metadata is absent, an existing marker can help, but a
   known expected size always wins over the marker.

8. Cast-failure cleanup:
   `do_play` must call `do_cleanup(&state)` on every error-return path that
   occurs after `start_webtorrent` and before the post-playback reaper is
   spawned. The two paths in scope today are the `Ok(Err(_))` and `Err(_)`
   branches around `cast::cast_url`: an unreachable Chromecast or a panic
   inside the spawn_blocking task otherwise leaves the just-started
   webtorrent + ffmpeg as orphans until the next play / `kill-workers` /
   restart. This is layer 8 and complements rather than replaces the
   pre-start cleanup at the top of `do_play`. NEVER reintroduce
   `do_cleanup` BETWEEN `start_webtorrent` and the transcode/cast step —
   it would SIGTERM the just-spawned worker and produce the
   `Connection refused` failure mode that motivated commit `4d3ef73`.

9. Cast health monitor:
   `cast_health_monitor` (server.rs) is a background tokio task spawned
   alongside the post-playback reaper for every chromecast play. After a
   10s startup grace, it polls `cast.get_info()` every 5s. When the
   Chromecast's `player_state` is reported as `Idle`/`Unknown`/empty (or
   the query itself fails) for 3 consecutive polls, the monitor declares
   the cast DEAD, runs `do_cleanup`, and exits. This catches the silent-
   failure class that no other defense layer sees: `cast_url()` returned
   OK, the receiver acknowledged the LOAD message, but the player engine
   never actually started ("blue cast icon" failure mode) — or started
   then ended unexpectedly because of a network blip, decoder error, app
   eviction, or ambient screensaver. Without this monitor, ffmpeg keeps
   transcoding into the void and `spela status` reports
   `running: true, status: streaming` while the TV shows the wallpaper.
   Identity is keyed off `app_state.current.started_at` (DateTime<Utc>),
   not webtorrent_pid, because Local Bypass plays use pid=0 and back-to-
   back local plays would otherwise be indistinguishable from the
   monitor's perspective. Worst-case detection latency: 10s grace + 3 ×
   5s polls = 25s.

## Systemd Drop-In

Use the tracked drop-in rather than overwriting the host's full service file:

```bash
sudo mkdir -p /etc/systemd/system/spela.service.d
sudo install -m 0644 systemd/spela-resource-limits.conf \
  /etc/systemd/system/spela.service.d/resource-limits.conf
sudo systemctl daemon-reload
sudo systemctl restart spela.service
```

This preserves host-specific environment and `ExecStart` configuration while
bounding the worker cgroup.

## Emergency Checklist

Use this only when the host is under pressure and WebTorrent workers appear
orphaned:

```bash
pgrep -af 'WebTorrent|webtorrent'
for pid in $(pgrep -f 'WebTorrent|webtorrent'); do
  ps -p "$pid" -o pid,ppid,user,stat,etime,pcpu,pmem,rss,comm,args
done
```

If the workers are stale and not owned by active playback, terminate them
gracefully:

```bash
spela kill-workers --human
# or, if the binary is not available:
pkill -TERM -f 'WebTorrent|webtorrent'
```

Do not use media cleanup commands unless deleting temporary playback artifacts
is intended.
