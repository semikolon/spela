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
- Every WebTorrent worker should be traceable to current playback state.
- At server startup, stale workers that are not owned by active playback should
  be reconciled and terminated with `SIGTERM`.
- `SIGKILL` is an escalation path only after a grace period and should be
  logged.
- Resource limits should be enforced outside the application as well, using a
  systemd unit, slice, or transient scope where possible.

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
