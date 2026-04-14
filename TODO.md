# Spela TODOs 🎬🍿

### Post-Search Optimization
- [ ] Implement Chromecast Receiver UI loading state (spinner/progress) to replace the "Black Flash" transition between video clips. 🎉🏙️
- [ ] Refine year-aware and quality-aware result prioritisation in `src/search.rs` to better handle franchise sequels (e.g., 2025 vs 2026). 🕵️‍♂️
- [ ] **TVTime "next unwatched episode" integration for Ruby** — Ruby should know which episodes the user has actually watched before guessing from search ranking. Tonight's failure mode: when user said "play the Boys episode" with no season/episode, Ruby chose the latest available (S05E02 — unwatched) instead of the next unwatched (S05E01). Architecture sketched in `~/.claude/hooks/conversation_engine.py` near `SPELA_TOOL_DECLARATION`: new `tvtime_client.py` with `get_show_progress(show)` + `mark_watched(show, s, e)`, new Gemini tool `get_next_episode(show)` registered alongside `run_spela`, system-prompt rule "before run_spela play on a TV show, always call get_next_episode unless the user explicitly named the season/episode", read-through 1h cache invalidated on every successful spela play of a TV show. Auth: TVTime removed their public API ~2019; reverse-engineered sidecar (`https://beta.tvtime.com/sidecar?o=https://api.tvtime.com/v1/...`) is the only stable route. Trakt.tv has an official API and many TVTime users sync to it — prefer Trakt if available. Currently blocked on user finding working credentials (Apr 15 reset-password-hell). Cross-reference: dotfiles commit `57b2d92`.

### System Hardening
- [x] Increase Node.js heap memory limits by default for webtorrent-cli to prevent "Ghost Crashes" during large file verification. Implemented in `src/torrent.rs` with `--max-old-space-size=4096`.
- [x] Add robust stale WebTorrent worker detection to prevent orphan workers from inflating disk, RAM, and swap usage. See `OPERATIONS.md`.
- [x] Add a worker-only cleanup command/path that terminates WebTorrent/ffmpeg without deleting media or rewriting playback history. Implemented as local `spela kill-workers`.
- [x] Reconcile stale WebTorrent workers on server startup before accepting new playback.
- [x] Add systemd/cgroup containment for media workers (`MemoryHigh`, `MemoryMax`, task limits, and CPU quota/weight where practical). Implemented as `systemd/spela-resource-limits.conf`.
- [x] Tighten local-bypass completion markers so `.spela_done` is written only when expected torrent byte size and physical file bytes agree.
- [ ] Add an external watchdog/alert that reports orphan WebTorrent workers and resource pressure before the host becomes unhealthy.
