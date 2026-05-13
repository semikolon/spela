# spela Operations

## Worker Safety

Since v3.3.0 (Apr 29, 2026), `spela` runs the BitTorrent client in-process via
librqbit; the Node `webtorrent-cli` subprocess is gone. Transcoding is still
delegated to `ffmpeg` subprocesses. Treat the active torrent (librqbit's
session-managed `TorrentId`) and the ffmpeg transcode workers as owned
resources with their own failure domain. They must not be allowed to outlive
playback indefinitely or consume the whole host.

The principles in this document were forged during a pre-v3.3.0 incident in
which abandoned WebTorrent (Node) workers consumed most host memory and swap
for more than a day; the host then appeared to have unrelated SSH, DNS, and
container problems because the entire userspace was under severe resource
pressure. The librqbit migration eliminated the Node-subprocess class of
orphan, but the underlying principles (worker / media cleanup separation,
sparse-aware disk accounting, traceable ownership, defense-in-depth) carry
over verbatim to ffmpeg workers and any future torrent backend.

## Invariants

- Process cleanup and media cleanup are separate operations.
- Emergency worker cleanup must terminate ffmpeg transcode workers (and any
  orphan pre-v3.3.0 webtorrent-cli processes) only; it must not delete
  media, mark files verified, or rewrite playback history. The librqbit
  session is in-process, so its analog is `session.delete(TorrentIdOrHash::Id(...))`
  — called via `TorrentEngine::stop(id, delete_files=false)` to drop the
  torrent from the active set while leaving bytes on disk for Local Bypass.
- User-facing playback cleanup may delete temporary transcode files, but that
  must stay explicit and separate from worker-only cleanup.
- `.spela_done` completion markers require a known expected byte size and a
  physical byte check. Never derive completion from playback duration alone.
- Every active torrent should be traceable to current playback state via
  `app_state.current.pid`. spela's `pid != 0` invariant uniquely identifies a
  librqbit-managed torrent — the `+1` shift over librqbit's `TorrentId`
  (which starts at 0) keeps `pid == 0` as the Local Bypass sentinel
  (`shift_librqbit_id` / `unshift_librqbit_id` in `torrent_engine.rs`).
- At server startup, stale resources that are not owned by active playback
  should be reconciled and terminated with `SIGTERM`. spela performs a
  one-shot webtorrent-cli orphan sweep on startup as a transition aid for
  hosts upgrading from pre-v3.3.0; ffmpeg orphans from prior crashes are
  caught by the `do_play` pre-start cleanup.
- `SIGKILL` is an escalation path only after a grace period and should be
  logged.
- Resource limits should be enforced outside the application as well, using a
  systemd unit, slice, or transient scope where possible.
- Worker spawn paths must always be paired with cleanup paths even on failure.
  Any error-return between `TorrentEngine::start` (or Local Bypass attach)
  and the post-playback reaper spawn is a leak unless it explicitly tears the
  worker pipeline down. Specifically: cast failure, transcode failure, the
  May 13 stream-start fail-fast trigger (20 s / 0 segments), or any panic in
  the early phase of `do_play` must call `do_cleanup(&state)` on the way out.
- Disk accounting must use allocated blocks, not logical length.
  Any BEP-53 file-selection mode (pre-v3.3.0 webtorrent's `-s <idx>`,
  current librqbit's `AddTorrentOptions { only_files: Some(vec![idx]) }`)
  creates sparse placeholder files whose `metadata.len()` reports the full
  multi-GB torrent size while only a few KB exist on disk. Summing `len()`
  will trip the media cap before any real download. Use
  `metadata.blocks() * 512` everywhere disk usage is measured
  (`disk.rs::dir_size`, `Local Bypass` health checks).
- Disk safety has two independent layers: spela's own media-dir cap
  (`MAX_MEDIA_MB`) protects spela from itself, and the host-filesystem
  free-space floor (`MIN_FS_FREE_MB`) protects the rest of the host from
  spela. Both checks must pass before a new download starts. The host floor
  is best-effort — `None` on `df` failure proceeds rather than blocks, so
  a parser regression can never make spela unusable on its own.
- `stream_host` must be resolvable BY THE CHROMECAST, not just by spela.
  Chromecast devices hardcode Google DNS (8.8.8.8 / 8.8.4.4) and ignore
  both DHCP option 6 and the LAN's recursive resolver, so a hostname like
  `darwin.home` will silently fail to resolve on the receiver even though
  every other LAN client sees spela just fine — the LOAD succeeds, the
  receiver never fetches the URL, and `player_state` stays IDLE forever.
  Two acceptable configurations:
  1. **LAN IP** (`192.168.4.1`) — always works, zero infrastructure.
  2. **Hostname + router-side DNS DNAT hijack** — redirect port 53 traffic
     from each Chromecast IP to the local resolver via iptables PREROUTING
     DNAT (see Darwin's `/etc/iptables/rules.v4`). This is what Darwin
     runs today so `stream_host = "darwin.home"` works end-to-end.
  spela startup WARNs if `stream_host` looks like a hostname — the warning
  is informational and can be ignored on hosts with the DNAT hijack in
  place. Confirmed live Apr 15, 2026 via tcpdump:
  `192.168.4.126.36919 > 8.8.8.8.53: A? www.google.com` (DNS sent to
  hardcoded Google DNS, intercepted by PREROUTING, rewritten to
  `192.168.4.1:53`).
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
   Provide a command or endpoint equivalent to `spela kill-workers` that
   sends `SIGTERM` to spela-owned ffmpeg transcode workers (and orphan
   pre-v3.3.0 webtorrent-cli processes) without touching files. Keep this
   separate from `spela stop`. The librqbit session shuts down with spela
   itself, so the torrent-side analog is `systemctl restart spela` or
   `TorrentEngine::stop(id, false)` for a single torrent.

3. Service containment:
   Run spela (and the embedded librqbit session + ffmpeg subprocesses) in a
   bounded cgroup or transient systemd scope. Implemented:
   `systemd/spela-resource-limits.conf` is a host-safe drop-in with
   `MemoryHigh`, `MemoryMax`, `MemorySwapMax`, `TasksMax`, and `CPUQuota`.
   librqbit's own bounded settings — `PeerConnectionOptions` timeouts
   (15 s connect, 60 s rw, 120 s keepalive) and `concurrent_init_limit = 4`
   — complement the cgroup limits at the in-process level.

4. Watchdog:
   Add an external host watchdog that checks memory pressure, swap pressure,
   and orphaned ffmpeg workers (the only orphan class possible since v3.3.0).
   It may alert early and may terminate only clearly orphaned workers under
   dangerous pressure. It must not delete media.

5. Startup reconciliation:
   On `spela server` startup, kill stale ffmpeg workers from previous sessions
   if they are not owned by the current state. This protects against
   abandoned SSH sessions and previous crashes. spela also performs a one-shot
   sweep for orphan pre-v3.3.0 webtorrent-cli processes (transition aid; safe
   to leave running indefinitely — no-ops on hosts that never had Node
   webtorrent installed). librqbit's session does not persist torrents across
   spela restarts, so there is no librqbit-side stale-state class to
   reconcile.

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
   occurs after `TorrentEngine::start` (or Local Bypass attach) and before
   the post-playback reaper is spawned. The paths in scope: the `Ok(Err(_))`
   and `Err(_)` branches around `cast::cast_url`, AND (since v3.4.0, May 13)
   the stream-start fail-fast trigger inside the HLS pre-buffer loop —
   `should_fail_fast_stream_start(elapsed, segments)` returns true at 20 s
   with 0 segments produced; the error is returned to `handle_play` which
   auto-fallbacks to the next search result. Without these explicit cleanup
   paths an unreachable Chromecast or a panic inside the spawn_blocking task
   leaves the just-started torrent + ffmpeg as orphans until the next play /
   `kill-workers` / restart. This is layer 8 and complements rather than
   replaces the pre-start cleanup at the top of `do_play`. NEVER reintroduce
   `do_cleanup` BETWEEN `TorrentEngine::start` and the transcode/cast step —
   it would tear down the just-started torrent (or SIGTERM the just-spawned
   ffmpeg) and produce the `Connection refused` failure mode that motivated
   commit `4d3ef73`.

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

Use this only when the host is under pressure and spela-owned ffmpeg workers
(or orphan pre-v3.3.0 webtorrent-cli processes on a host that never restarted
post-migration) appear orphaned:

```bash
# Current-version target: ffmpeg subprocesses parented by spela.
# Backwards-compat target: any lingering Node webtorrent-cli from pre-v3.3.0.
pgrep -af 'ffmpeg.*\(transcoded_hls\|spela\)|WebTorrent|webtorrent'
for pid in $(pgrep -f 'ffmpeg.*\(transcoded_hls\|spela\)|WebTorrent|webtorrent'); do
  ps -p "$pid" -o pid,ppid,user,stat,etime,pcpu,pmem,rss,comm,args
done
```

If the workers are stale and not owned by active playback, terminate them
gracefully:

```bash
spela kill-workers --human
# Equivalent shell fallback if the binary isn't available:
pkill -TERM -f 'ffmpeg.*\(transcoded_hls\|spela\)|WebTorrent|webtorrent'
# librqbit session is in-process; if the spela binary itself is wedged,
# a full `sudo systemctl restart spela` is the canonical recovery.
```

Do not use media cleanup commands unless deleting temporary playback artifacts
is intended.
