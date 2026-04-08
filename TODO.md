# Spela TODOs 🎬🍿

### Post-Search Optimization
- [ ] Implement Chromecast Receiver UI loading state (spinner/progress) to replace the "Black Flash" transition between video clips. 🎉🏙️
- [ ] Refine year-aware and quality-aware result prioritisation in `src/search.rs` to better handle franchise sequels (e.g., 2025 vs 2026). 🕵️‍♂️

### System Hardening
- [x] Increase Node.js heap memory limits by default for webtorrent-cli to prevent "Ghost Crashes" during large file verification. Implemented in `src/torrent.rs` with `--max-old-space-size=4096`.
- [x] Add robust stale WebTorrent worker detection to prevent orphan workers from inflating disk, RAM, and swap usage. See `OPERATIONS.md`.
- [x] Add a worker-only cleanup command/path that terminates WebTorrent/ffmpeg without deleting media or rewriting playback history. Implemented as local `spela kill-workers`.
- [x] Reconcile stale WebTorrent workers on server startup before accepting new playback.
- [ ] Add systemd/cgroup containment for media workers (`MemoryHigh`, `MemoryMax`, task limits, and CPU quota/weight where practical).
- [ ] Add an external watchdog/alert that reports orphan WebTorrent workers and resource pressure before the host becomes unhealthy.
