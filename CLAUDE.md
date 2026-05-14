# spela — AI-Agent-Ready Media Controller

## NO PERSONAL DATA (MANDATORY)
**Public repo.** Never commit real names, IPs, API keys, device names, or household details to code/docs. Use placeholders. Runtime config (`config.toml`, `state.json`) is user-local and gitignored.

## Overview

Rust CLI + HTTP API server for torrent-to-Chromecast streaming. Single 5.2MB binary, zero runtime deps on target (ffmpeg only; BitTorrent client is embedded via librqbit since v3.3.0).

**Status**: v3.4.3 on Darwin (May 14, 2026). HLS cast pipeline, Smart Resume, self-healing disk cache, **5-tier Sarpetorp-tuned ranker** with **30× seed-disparity override at tier 4** + **transitivity-safe `effective_res_tier`** (1080p target, 2160p demoted below 480p, DV hard gate, weak-swarm H.264 yields to strong-swarm HEVC), cast_health_monitor with **pause-gated auto-recast** (once user pauses in a session, recast is disabled — no more surprise auto-resume after CrKey 1.56's Paused→IDLE receiver-app unload), DNS DNAT hijack for Chromecast LAN hostname resolution, **20s HLS pre-buffer fail-fast with auto-retry to next search result**, **head-of-stream probe in `serve_torrent_stream`** so promised Content-Length is never under-delivered, **`spela seek` takes absolute episode position** (no-arg = HWM resume; pre-stream-start error points at `spela play --seek` for re-transcode). **v3.5.0 HLS cache foundation shipped opt-in** (`hls_cache_cap_mb = 0` default; cache-HIT short-circuit deferred to v3.5.1 — see Hard-Won Lessons). Native cast.seek works for in-flight forward/backward seeks within the current transcode window; pre-transcode-start seeks require server-side restart via `spela play --seek N`; Custom Receiver App still blocked on $5 registration.

**Architecture**: `spela server` on Darwin (`darwin.home:7890`). `spela <command>` thin HTTP client from any LAN machine. Voice assistants call HTTP API directly.

## Usage

```bash
spela search "Good Luck Have Fun Dont Die"       # Auto-detects movie vs TV
spela search "legion" --season 1 --episode 5   # Search + rank results
spela play 1                                     # Play result #1 (auto: magnet, file_index, metadata)
spela play 1 --cast "Vardagsrum"                 # Play to living room TV
spela play 1 --no-intro                          # Skip intro bumper
spela next                                       # Next episode
spela pause / resume / stop                      # Playback control
spela status / history / targets                 # Info
```

`spela play <N>` is the primary flow. Carries all metadata (file_index, IMDB, show, season, episode) automatically from the last search.

## Key Files

- `src/main.rs` — CLI (clap) + server startup. Detects `play 1` (result ID) vs `play magnet:...`
- `src/server.rs` — axum HTTP API, 21 endpoints (including Custom Receiver: cast-config, seek-restart, position, retry). Orchestrates search→play→cast pipeline
- `src/search.rs` — TMDB + Torrentio. **Auto-detect movie vs TV** via TMDB multi-search. **5-tier Sarpetorp-tuned ranker** (v3.4.0, shared `pub fn rank_results_mut`): (1) single-file > pack, (2) non-DV > DV (HARD gate — GTX 1650 NVENC can't parse DV profile 5/7 RPU), (3) target resolution `1080p > 720p > 480p > 2160p > unknown` with ≥50-seed viability (2160p dead-last because TVs max at 1080p native + 4K NVENC transcode is ~3x heavier), (4) H.264 > HEVC within same resolution (insta-play tiebreak, ≥5-seed threshold) **WITH v3.4.0 seed-disparity override** — when the HEVC alternative has ≥30× the seeds of the H.264 tier-4 winner, promote HEVC (anchored to the May 13 Cinecalidad-99 vs MeGusta-7116 incident; below 30× the codec-cost tradeoff isn't worth flipping), (5) more seeds. The v3.0.0 → v3.1.0 flip: resolution tier was promoted ABOVE codec preference because HEVC→H.264 transcode at 1080p runs ~6x realtime, fast enough to absorb the cold-start cost — so 1080p HEVC now beats 720p H.264 on a 1080p TV
- `src/cast.rs` — Native Chromecast via rust_cast + mdns-sd. 3x retry + IP cache + known device fallback
- `src/torrent_engine.rs` — pure-Rust torrent engine wrapping `librqbit::Session` (since v3.3.0 / Apr 30, 2026). spela-shaped surface: `start` / `progress` / `handle` / `stop`, `validate_magnet_uri` (SSRF defense), `shift_librqbit_id` / `unshift_librqbit_id` (preserves the `pid==0 = Local Bypass` sentinel against librqbit's TorrentId-starts-at-0). Opens `Session::new_with_opts` with peer-connection timeouts (15s/60s/120s) + concurrent_init_limit=4
- `src/torrent_stream.rs` — axum HTTP Range handler that pipes `librqbit::FileStream` (`AsyncRead + AsyncSeek`) into `axum::body::Body` via `tokio_util::io::ReaderStream`. Pure `parse_range_header` covers all RFC 7233 forms (open-ended, bounded, suffix, multi-range, malformed). The `/torrent/{id}/stream/{file_idx}` route is **loopback-only** via the `require_loopback_source` middleware (H4)
- `src/torrent.rs` — slimmed to ffmpeg-PID utilities + a one-shot post-upgrade webtorrent-orphan sweep at startup. Pre-v3.3.0 this wrapped the Node `webtorrent-cli` subprocess
- `src/transcode.rs` — ffprobe codec+duration detection, ffmpeg audio transcode (EAC3/DTS/AC3→AAC) + video transcode (HEVC→H.264 via NVENC) + intro concat
- `src/subtitles.rs` — Stremio OpenSubtitles v3 (zero auth), SRT→WebVTT
- `src/disk.rs` — two-layer disk safety: 10 GB internal media cap (sparse-aware via `metadata.blocks() * 512`) + 20 GB host-filesystem free-space floor (via `df -Pk`) + age-based prune (top-level files AND directories) + **LRU pressure eviction via `prune_to_fit`** so the cap is a self-maintaining upper bound instead of a refusal wall (Apr 15, 2026). `title_matches_active` tokenizes filename vs active title so `The.Boys.S05E03.FLUX.mkv` correctly matches `The Boys S05E03`. See `OPERATIONS.md` for the full defense-in-depth map
- `src/state.rs` — state.json + last_search.json (play-by-id) + resume_positions (per-episode keyed for TV shows via `resume_position_key` + `extract_se_suffix` — Apr 15, 2026 fix for IMDb-ID collision across episodes of the same show). v3.5.0 adds `CurrentStream.cache_key: Option<String>` populated at `do_play` for cacheable plays (set when caching is enabled + `imdb_id` known + `ss_offset==0.0`).
- `src/hls_cache.rs` — v3.5.0 HLS cache foundation (module-only; full design + lifecycle in the module's `//!` header doc). Pure helpers: `build_cache_key`, `cache_dir_for_key`, `cache_root`, `is_cache_hit`, `mark_complete`, `cache_dir_size_bytes` (sparse-aware via `metadata.blocks() * 512`), `cache_entries_by_age`, `prune_cache_to_fit` (LRU). Cache-FILL wired in `do_cleanup` via `try_promote_to_hls_cache`; cache-HIT short-circuit deferred to v3.5.1 (blocker: synthetic-master-playlist vs multi-variant reconciliation — see Hard-Won Lessons).
- `static/cast-receiver.html` — Custom Cast Receiver (Shaka Player + CAF v3, ~200 lines)
- `src/config.rs` — `~/.config/spela/config.toml`. **Hardcoded XDG path on every platform** because Rust's `dirs::config_dir()` returns `~/Library/Application Support` on macOS, which silently hid the Mac CLI's user config and dropped it back to `localhost:7890`
- `OPERATIONS.md` — worker safety plan, stale WebTorrent prevention, emergency cleanup rules

## Build & Deploy

```bash
# Repo cloned on Darwin at ~/Projects/spela (Rust 1.94):
cd ~/Projects/spela && git push   # from Mac
ssh darwin 'cd ~/Projects/spela && git pull && cargo build --release'
ssh darwin 'sudo systemctl restart spela'
```

`~/.local/bin/spela` on Darwin is a symlink to `~/Projects/spela/target/release/spela` (verified Apr 30, 2026 — `lrwxrwxrwx ... -> /home/fredrik/Projects/spela/target/release/spela`). `cargo build --release` rewrites the inode the symlink points at; the running spela process keeps the OLD inode open until `systemctl restart` re-execs against the symlink. **No `cp` step needed** — earlier versions of this doc had `cp ~/Projects/spela/target/release/spela ~/.local/bin/` which errors with "are the same file" (Apr 30 deploy hit this).

**Darwin working-tree drift recovery** (May 13, 2026, observed mid-deploy): if `git pull` fails on Darwin with *"cannot pull with rebase: You have unstaged changes"* AND `git diff --stat HEAD` shows ≥1000 LOC across multiple source files, do NOT `git stash`, do NOT force-discard. The likely cause is past-session edits made directly on Darwin (incident-debug hotpatches) that were later committed FROM Mac Mini, leaving Darwin's working tree dirty against an older HEAD pointer. Forensic procedure: (1) `git rev-parse HEAD origin/master` on Darwin to find Darwin's HEAD; (2) on Mac Mini, `git log --oneline <darwin-HEAD>..HEAD` to enumerate the commits Darwin doesn't have yet; (3) `sha1sum` each Darwin WT file against `git show <candidate-commit>:<file>` on Mac Mini for the same path — match means the WT content is byte-identical to a master commit, safe to discard. If all files match (or match + formatting-only deltas), `git checkout -- <files>` on Darwin is non-destructive — content is preserved in master. If even one file shows unique content, STOP and propose committing-from-Darwin to a branch instead. May 13 anchor: 8 files changed on Darwin, 7 matched `ab9a9a5` byte-for-byte, 1 (`search.rs`) was an older snapshot superseded by a `cargo fmt` pass on Mac — all features preserved in master, safe discard, deploy completed cleanly.

## Mac Mini (Client)

- **Symlink**: `~/.local/bin/spela` → `~/Projects/spela/target/release/spela` (rebuilds auto-update the symlink target)
- **Default server**: local user config typically points at `darwin.home:7890`; the public codebase itself should keep generic defaults and examples
- **Ruby voice assistant**: Can control spela via `run_spela` tool (Gemini function declaration in `conversation_engine.py`). Example: "Hey Ruby, play Legion season one episode two"

## Darwin (Server)

- **Binary**: `~/.local/bin/spela`
- **Service**: `/etc/systemd/system/spela.service` (auto-start, restart on crash)
- **Config**: `~/.config/spela/config.toml` (TMDB key, default device, stream host)
- **State**: `~/.spela/` (state.json, last_search.json, devices.json, webtorrent.log)
- **Intro**: `~/.config/spela/intro.mp4` (5s Kling AI-generated bumper, 1080p H.264+AAC)
- **Media**: `~/media/` (temporary, 10GB cap, 24h auto-cleanup). Transcoded files cleaned on stop
- **Deps**: webtorrent-cli (mise/npm), ffmpeg (apt)
- **Firewall**: ports 8888 (webtorrent HTTP) + 7890 (spela API) open to LAN in nftables
- **PATH**: spela's systemd unit uses minimal `/usr/local/bin:/usr/bin:/bin` since v3.3.0 (post-librqbit, no Node subprocess). Pre-v3.3.0 needed mise shims for `webtorrent` — that PATH leg was removed Apr 30, 2026 along with the global `npm uninstall -g webtorrent-cli`.

## Hard-Won Lessons (preemptive architecture gotchas — read before touching these areas)

Incident narratives + RCA + journal evidence: `git log` (every fix-commit carries the detailed analysis). Key SHAs: `7cd71c6` v3.4.0 bad-source trio · `c3b41a0` v3.4.1 ranker transitivity · `e0e81bf` v3.4.2 pause-gate · `e3960aa` v3.4.3 seek semantics · `40d3b96` v3.5.0 cache foundation · `ee38f92` 10-bit NVENC · `dcbaed7` CORS revert · `2c598f4` dual-bind. Deep dives: `docs/INCIDENT_REPORT_WILDERPEOPLE_2026_05_01.md`, `docs/CAST_DEBUGGING_RECIPES.md`.

**Ranker (`rank_results_mut`)**: 5 tiers via `effective_res_tier` — bakes seed-viability INTO resolution bucket so the comparator is a strict total order. *Pairwise threshold-fallthrough rules are a non-transitive comparator class bug.* 30× HEVC seed-disparity override (`SEED_DISPARITY_OVERRIDE`). DV demoted (NVENC GTX 1650 can't decode profile 5/7 RPU). `filter_results_by_show_title` token-based pre-ranker. `is_hevc_from_title` normalizes codec separators (`H 265`, `h.265`, `H-265` all match). Sarpetorp policy: 1080p > 720p > 480p > 2160p > unknown (TVs max 1080p native).

**cast_health_monitor (`src/server.rs`)**: identity = `started_at` not `torrent_pid` (Local Bypass pid=0 collisions). `paused_seen_in_session` HARD GATE on `should_attempt_recast` — sticky, cleared only by stream replacement; CrKey 1.56 unloads receiver app after ~16 min pause (Paused→IDLE looks like wedge). BUFFERING transient (don't kill) but bounded by `MAX_BUFFERING_DURATION_SECS=60s`. Auto-recast frequency-capped (`RECAST_COOLDOWN_SECS=30s`), not lifetime cap — receiver flakes recur every 15-30 min and each recast IS recovery. `is_idle_in_cold_start_window` protects 60s startup (DMR media-session allocation can take 25-60s). `HWM_CLEAR_FRACTION=0.96` is the ONLY threshold for HWM-clear-at-near-EOF, NOT kill — *one constant for two policies is a load-bearing coincidence*. `is_position_jump_suspicious(delta_wall, delta_abs)` rejects physically-impossible advances (stale current_time on stream replacement).

**do_play invariants (`src/server.rs`)**: NEVER reintroduce `do_cleanup` between `TorrentEngine::start` and transcode/cast step — SIGTERMs the just-spawned worker. `do_cleanup` on every error-return path after `TorrentEngine::start` (else orphans). Auto-resume from HWM via `app_state.get_position` unless explicit `--seek N` (which clears HWM — *explicit user actions override remembered state*). `should_fail_fast_stream_start(elapsed, segments)` fires at 20s/0 → `handle_play` auto-retry bumps `result_id` (distinct from cast_health_monitor cold-start IDLE — upstream-side, fires BEFORE LOAD).

**`spela seek` (v3.4.3)**: takes absolute episode position. `compute_cast_seek_target(absolute_pos, ss_offset, duration)` translates user-facing absolute → stream-relative at the API boundary. No-arg = HWM resume. `BeforeStreamStart` error → use `spela play --seek N` for re-transcode. *Generic lesson: CLI/API positions in user mental model, not implementation coordinate systems; translate at the boundary.*

**`ss_offset` / Smart Resume (`src/state.rs`)**: `CurrentStream.ss_offset` = `-ss N` passed to ffmpeg. Absolute source-timeline = `chromecast_time + ss_offset`. ONLY non-zero on transcoded plays — for non-transcoded, post-LOAD `cast.seek` adjusts current_time, so adding ss_offset double-counts. `resume_position_key` parses `SxxExx` from title and appends `_sXXeYY` (TV episodes share show imdb_id — without suffix, S05E02's HWM stomped S05E03's auto-resume). End-of-episode branch calls `reset_position` before `do_cleanup` (belt-and-suspenders).

**HLS pipeline → Chromecast**: cast LOAD URL = `/hls/master.m3u8` (synthetic via `handle_hls_master` — CrKey 1.56 refuses bare media playlist without CODECS/RESOLUTION/BANDWIDTH). `StreamType::Buffered` (NOT Live) — with Live, DMR ACKs LOAD but never fetches manifest. MPEG-TS segments (rust_cast's `Media` struct can't pass `hlsSegmentFormat` for fmp4). HLS v3-v4 only — avoid `-hls_playlist_type event` and `-hls_flags independent_segments` (bump to v6, Shaka on CrKey 1.56 can't parse). CORS `allow_origin(Any)` REQUIRED — Cast Receiver runs on `gstatic.com`, MSE strictly enforces CORS (Host-header allowlist is the real DNS-rebinding defense; tightening CORS broke MSE silently, May 1 incident). Pre-buffer ≥10 segments OR `#EXT-X-ENDLIST` (Reliability Mode for Local Bypass, waits for complete VOD set).

**ffmpeg + NVENC (`src/transcode.rs`)**: every filter chain feeding `h264_nvenc` MUST append `format=yuv420p` — NVENC h264 cannot encode 10-bit input (HEVC sources from MeGusta/ELiTE/d3g often Main 10, zero output, receiver IDLEs at `<init>`). `-profile:v high -level:v 4.0` matches synthetic master's hardcoded `avc1.640028`. `-map 0:a:{audio_index}` from `detect_codecs::CodecInfo` — DEFAULT track often Russian on dual-audio releases. `shift_srt` rewrites SRT timestamps by `-N` seconds before ffmpeg reads them (input-seek resets PTS, `subtitles` filter reads cue times literally). CRLF normalize `
` → `
` before SRT split (`

` separator silently fails `split("

")`). `tokio::spawn(child.wait())` not `mem::forget(child)` — zombie reaping.

**librqbit (`src/torrent_engine.rs`)**: `CryptoProvider::install_default()` early in `main()` (aws-lc-rs provider) or rustls 0.23 panics + poisons mutex cascade across axum tasks. `shift_librqbit_id` adds +1 — TorrentId starts at 0, collides with pid=0 Local Bypass sentinel. `/torrent/{id}/stream/{file_idx}` loopback-only via `require_loopback_source` middleware (URL builder hardcodes `127.0.0.1`). `serve_torrent_stream` retries on `"initializing"` for up to 30s + does a 64 KiB head-of-stream probe before Body construction (librqbit `FileStream::poll_read` blocks on missing pieces but may error transiently). `validate_magnet_uri` at HTTP boundary — SSRF defense (`librqbit::AddTorrent::Url` accepts http(s):// and fetches them). `lock_recover()` not `.lock().unwrap()` — poison cascade safety in long-running async server. File selection: `AddTorrentOptions { only_files: Some(vec![idx]) }`.

**Local Bypass + disk safety (`src/server.rs`, `src/disk.rs`)**: `find_local_bypass_match` (title/year/quality/health decision matrix). `top_level_file_is_healthy` trusts title-match for single-file releases (≥100 MB, non-sparse); directory bypass keeps strict size match (disambiguates season packs). `.spela_done` requires byte evidence, never duration. `dir_size` uses `metadata.blocks() * 512` via `MetadataExt` (sparse-aware — webtorrent/librqbit BEP-53 file selection creates sparse placeholders with full logical size). `MAX_MEDIA_MB=10_000` internal cap + `MIN_FS_FREE_MB=20480` host floor via `df -Pk`. `prune_to_fit` LRU-evicts oldest until under cap (refusal-wall → self-maintaining). `AppState.corrupt_files` HashSet skip-list populated by `inspect_ffmpeg_log_for_corruption` post-mortem (EBML / HEVC-ref / `dup>100` signals).

**HLS cache (`src/hls_cache.rs`) [v3.5.0 foundation, opt-in]**: cache-FILL in `do_cleanup::try_promote_to_hls_cache` — atomic rename of `transcoded_hls/` → `<media_dir>/hls_cache/<key>/` when `ss_offset==0.0` + `#EXT-X-ENDLIST` present + cache dir not already populated. Cache-HIT short-circuit **deferred to v3.5.1**. Blocker: `handle_hls_master` synthesizes single-variant master (references `playlist.m3u8`) while ffmpeg's `-var_stream_map` writes multi-variant (`stream_0.m3u8` + `stream_1.m3u8`) — needs either dedicated `/hls_cache/{key}/{file}` route OR master-handler awareness pass. Default disabled (`hls_cache_cap_mb=0`). Cache key: `<imdb_id>_s<NN>e<MM>_<lang>_<intro|nointro>_v<VERSION>`.

**CrKey 1.56 quirks**: hardcoded Google DNS (8.8.8.8 / 8.8.4.4) ignores DHCP option 6 — for hostname `stream_host` to work, router-side DNAT hijack via iptables PREROUTING per-Chromecast-IP rewrite of UDP/TCP port 53 to local AdGuard (persisted in `/etc/iptables/rules.v4`; standard Pi-hole technique). 1.56.x is terminal firmware for 1st/2nd-gen Chromecasts — receiver app unloads after ~16 min pause (Paused→IDLE), drops connection after `cast_url()` returns (no visibility without cast_health_monitor polling), `hlsSegmentFormat` not exposable via rust_cast's `Media` struct. **DMR overlay is stream-type-dependent, NOT metadata-dependent**: progress-bar UI renders iff HLS is in VOD mode (`#EXT-X-ENDLIST` present); live mode = no progress bar. `vod_manifest_padded=false` + `experimental_endlist_hack=false` preserves the no-overlay live-mode UX.

**Worker safety + bind**: `spela kill-workers` terminates ffmpeg transcode workers (+ orphan pre-v3.3.0 webtorrent-cli) only — never media. Dual-bind `127.0.0.1` + `--host` so ffmpeg's `127.0.0.1:7890` URL works regardless of `--host` (WAN exposure remains impossible — `--host` stays specific).

**Ruby integration (`~/.claude/hooks/conversation_engine.py`)**: `run_spela` tool defaults to `args='1'` — Gemini was inventing picks from shiny-label bias (4K HDR DV HEVC over 1080p H.264). Spela's ranker is authority on which torrent to play. `_execute_spela` synthesizes TTS confirmation BEFORE the spela subprocess call (engine-side enforcement; soft-prompt was unreliable — Gemini misinterpreted "say confirmation BEFORE tool call" as "stop after confirmation").

**Config**: Mac CLI hardcodes `~/.config/spela/config.toml` — `dirs::config_dir()` returns `~/Library/Application Support` on macOS, hiding user config. Same gotcha applies to any cross-platform Rust app using the `dirs` crate. spela startup WARNs on hostname `stream_host` — generic safety net for deployments without the DNAT hijack.

**Intro clip**: default-OFF (renamed `~/.config/spela/intro.mp4.disabled`) pending concat-buffer-stall bug fix. Per-call `--intro`/`--no-intro` overrides still work. When enabled: prepended via ffmpeg concat filter, both streams scaled to 1080p, 45s pre-buffer (vs 25s without intro). **Gotcha**: `-dn -map_metadata -1` required — concat produces `bin_data` streams Chromecast rejects.


## Chromecast Devices

Hardcoded fallback IPs in `src/cast.rs`:
- Fredriks TV: 192.168.4.126
- Vardagsrum: 192.168.4.58

DNS: `darwin.home` → darwin.home (AdGuard Home rewrite, configured Mar 18)

## See also

- [`~/dotfiles/docs/shannon_bedroom_kiosk_plan_2026_05_06.md`](~/dotfiles/docs/shannon_bedroom_kiosk_plan_2026_05_06.md) — bedroom Shannon as local renderer + kiosk. Picks up the "Shannon could run mpv/Kodi/spela-as-local-player to bypass the wedge class entirely" thread from this doc's CrKey 1.56 EOL section. Spela local-renderer mode is Phase 7 of that plan.
