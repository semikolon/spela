# Spela TODOs 🎬🍿

### Post-Search Optimization
- [ ] Implement Chromecast Receiver UI loading state (spinner/progress) to replace the "Black Flash" transition between video clips. 🎉🏙️
- [ ] Refine year-aware and quality-aware result prioritisation in `src/search.rs` to better handle franchise sequels (e.g., 2025 vs 2026). 🕵️‍♂️
- [ ] **TVTime "next unwatched episode" integration for Ruby** — Ruby should know which episodes the user has actually watched before guessing from search ranking. Tonight's failure mode: when user said "play the Boys episode" with no season/episode, Ruby chose the latest available (S05E02 — unwatched) instead of the next unwatched (S05E01). Architecture sketched in `~/.claude/hooks/conversation_engine.py` near `SPELA_TOOL_DECLARATION`: new `tvtime_client.py` with `get_show_progress(show)` + `mark_watched(show, s, e)`, new Gemini tool `get_next_episode(show)` registered alongside `run_spela`, system-prompt rule "before run_spela play on a TV show, always call get_next_episode unless the user explicitly named the season/episode", read-through 1h cache invalidated on every successful spela play of a TV show. Auth: TVTime removed their public API ~2019; reverse-engineered sidecar (`https://beta.tvtime.com/sidecar?o=https://api.tvtime.com/v1/...`) is the only stable route. Trakt.tv has an official API and many TVTime users sync to it — prefer Trakt if available. Currently blocked on user finding working credentials (Apr 15 reset-password-hell). Cross-reference: dotfiles commit `57b2d92`.

### Cast Pipeline Rework — IN PROGRESS (Apr 15, 2026 overnight)
- [ ] **HLS rework for `/stream/transcode`** — the chunked-transfer fragmented MP4 endpoint is fundamentally incompatible with Chromecast Default Media Receiver. Confirmed live by `cast_health_monitor` on Apr 15, 2026: every cast attempt produced healthy ffmpeg + perfect Local Bypass + `cast_url()` returning OK, while the TV stayed on the blue cast icon and `player_state=IDLE`. Default Media Receiver's MP4 parser refuses the combination of `Transfer-Encoding: chunked` + no `Content-Length` + always-200 (never-206) responses. Workaround that proves the diagnosis: the same FLUX file remuxed via `ffmpeg -c:v copy -c:a aac -movflags +faststart` and served by `catt` (which uses `206 Partial Content` with a real `Content-Length`) plays perfectly on the same Chromecast / same network.

  **Architecture**: rewrite `transcode.rs` to output HLS instead of fMP4: `ffmpeg ... -f hls -hls_time 6 -hls_list_size 0 -hls_playlist_type event -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 -hls_segment_filename seg_%05d.m4s playlist.m3u8`. Add new axum endpoints `/hls/playlist.m3u8` and `/hls/segment/<name>` that serve the manifest + fmp4 init segment + .m4s segments with proper `Content-Length` and `206 Range` support. The cast LOAD URL becomes `http://darwin.home:7890/hls/playlist.m3u8` with content-type `application/vnd.apple.mpegurl`. Default Media Receiver supports HLS natively. The post-playback reaper still tracks ffmpeg, `cast_health_monitor` still tracks the receiver — both unchanged.

  **Trade-off analysis (Apr 15, 2026)**:

  *Disadvantages*: (1) ~5-10 sec cold-start vs current ~3-5 sec broken — HLS needs manifest + init + 1-2 segments before cast can start, ~1-2 sec wall per segment at NVENC's 6x realtime. (2) ~640 small HTTP requests over a 64-min episode instead of one long-lived chunked response (~100µs routing overhead each in axum, negligible). (3) ~640 small `.m4s` files in `~/media/transcoded_hls/` (negligible inode usage). (4) ~150-300 LOC net vs current. (5) ~1% container overhead from per-segment `moof` boxes. (6) Cleanup race window if a play stops while ffmpeg is mid-segment-write — `kill_pid` SIGTERMs ffmpeg cleanly so this is rare; worst case is one stale `.m4s` file deleted on next play.

  *Advantages*: (1) **It actually works on Default Media Receiver** — the entire reason. HLS is what Shaka Player (which DMR uses internally) is built around. (2) Better pause/resume — manifest survives HTTP connection drops, Chromecast can re-fetch and resume from current segment. Current fMP4 design treats long pauses as connection EOF. (3) Better seekability eventually — with `hls_playlist_type=event` the manifest is appendable during transcode and ENDLIST is written when ffmpeg finishes; at that point Chromecast can seek to any segment boundary. fMP4 has no byte index, seeking is impossible (this is exactly why spela has been chasing the Custom Cast Receiver workaround). (4) Standard format — works in VLC, mpv, iOS, Apple TV, web browsers. fMP4 chunked-transfer is bespoke. (5) Custom Cast Receiver becomes simpler — Shaka Player does HLS out of the box, just set `media.contentType = 'application/x-mpegurl'` and Shaka handles seeking, ABR, recovery. (6) `cast_health_monitor` works the same way (orthogonal change).

  *Show-stoppers*: none. Downsides are minor or already true. Going for it.

  *Production workaround until HLS lands*: `catt -d "Fredriks TV" cast /tmp/<remuxed>.mp4` directly on Darwin. Documented in spela CLAUDE.md and global CLAUDE.md.

  *Implementation status (Apr 15, 2026 overnight)*:
  - [x] `transcode_hls()` function in `src/transcode.rs` (mirrors filter chain of `transcode()`, swaps the muxer for HLS event playlist)
  - [ ] `serve_static_with_range()` helper in `src/server.rs` for proper Content-Length + 206 Range
  - [ ] `/hls/playlist.m3u8`, `/hls/init.mp4`, `/hls/{segment}` axum routes
  - [ ] `do_play` switched to `transcode_hls()` + cast URL `/hls/playlist.m3u8` + content-type `application/vnd.apple.mpegurl`
  - [ ] HLS-aware pre-buffer (wait for manifest + init + ≥1 segment instead of "5 MB ready")
  - [ ] `do_cleanup` deletes `transcoded_hls/` directory in addition to `transcoded_aac.mp4`
  - [ ] `kill_spela_ffmpeg_workers` pattern updated to also match `ffmpeg.*transcoded_hls`
  - [ ] Live test against Fredriks TV (TV off — user asleep)
  - [ ] Docs updated: spela CLAUDE.md, OPERATIONS.md, README.md
  - [ ] Test cases for `transcode_hls()` and the HLS endpoints

  Cross-references: spela `dd111ee` (the cast_health_monitor that surfaced this), `8735ea4` (cast-failure cleanup defense), [Igalia/cog#463](https://github.com/Igalia/cog/issues/463) (upstream confirmation that chunked + MP4 = broken player parsing).

### System Hardening
- [x] Increase Node.js heap memory limits by default for webtorrent-cli to prevent "Ghost Crashes" during large file verification. Implemented in `src/torrent.rs` with `--max-old-space-size=4096`.
- [x] Add robust stale WebTorrent worker detection to prevent orphan workers from inflating disk, RAM, and swap usage. See `OPERATIONS.md`.
- [x] Add a worker-only cleanup command/path that terminates WebTorrent/ffmpeg without deleting media or rewriting playback history. Implemented as local `spela kill-workers`.
- [x] Reconcile stale WebTorrent workers on server startup before accepting new playback.
- [x] Add systemd/cgroup containment for media workers (`MemoryHigh`, `MemoryMax`, task limits, and CPU quota/weight where practical). Implemented as `systemd/spela-resource-limits.conf`.
- [x] Tighten local-bypass completion markers so `.spela_done` is written only when expected torrent byte size and physical file bytes agree.
- [ ] Add an external watchdog/alert that reports orphan WebTorrent workers and resource pressure before the host becomes unhealthy.
