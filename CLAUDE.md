# spela — AI-Agent-Ready Media Controller

## NO PERSONAL DATA (MANDATORY)
**Public repo.** Never commit real names, IPs, API keys, device names, or household details to code/docs. Use placeholders. Runtime config (`config.toml`, `state.json`) is user-local and gitignored.

## Overview

Rust CLI + HTTP API server for torrent-to-Chromecast streaming. Single 6.9MB binary, zero runtime deps on target (except webtorrent-cli and ffmpeg).

**Status**: v2.0.0 deployed on Darwin. Casting stable, pause/resume works, intro clip, HEVC transcode. Seeking blocked on Custom Receiver App (next step).

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
- `src/search.rs` — TMDB + Torrentio. **Auto-detect movie vs TV** via TMDB multi-search. **4-tier ranking**: single-file > H.264 (≥5 seed threshold) > non-Dolby-Vision > more seeds. DV demotion is intra-codec-class — handles GTX 1650 NVENC's DV profile 5/7 RPU parse failure
- `src/cast.rs` — Native Chromecast via rust_cast + mdns-sd. 3x retry + IP cache + known device fallback
- `src/torrent.rs` — webtorrent-cli subprocess management with `-s <fileIdx>` for file selection
- `src/transcode.rs` — ffprobe codec+duration detection, ffmpeg audio transcode (EAC3/DTS/AC3→AAC) + video transcode (HEVC→H.264 via NVENC) + intro concat
- `src/subtitles.rs` — Stremio OpenSubtitles v3 (zero auth), SRT→WebVTT
- `src/disk.rs` — two-layer disk safety: 10 GB internal media cap (sparse-aware via `metadata.blocks() * 512`) + 20 GB host-filesystem free-space floor (via `df -Pk`) + 24h smart prune. See `OPERATIONS.md` for the full defense-in-depth map
- `src/state.rs` — state.json + last_search.json (play-by-id) + resume_positions (IMDB→seconds)
- `static/cast-receiver.html` — Custom Cast Receiver (Shaka Player + CAF v3, ~200 lines)
- `src/config.rs` — `~/.config/spela/config.toml`. **Hardcoded XDG path on every platform** because Rust's `dirs::config_dir()` returns `~/Library/Application Support` on macOS, which silently hid the Mac CLI's user config and dropped it back to `localhost:7890`
- `OPERATIONS.md` — worker safety plan, stale WebTorrent prevention, emergency cleanup rules

## Build & Deploy

```bash
# Repo cloned on Darwin at ~/Projects/spela (Rust 1.94):
cd ~/Projects/spela && git push   # from Mac
ssh darwin 'cd ~/Projects/spela && git pull && cargo build --release'
ssh darwin 'sudo systemctl stop spela && cp ~/Projects/spela/target/release/spela ~/.local/bin/ && sudo systemctl start spela'
```

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
- **PATH**: systemd service needs mise shims path for webtorrent

## Hard-Won Lessons

- **Worker cleanup must not imply media cleanup** — WebTorrent/ffmpeg are owned workers with their own failure domain. Emergency cleanup should terminate stale workers only; it must not delete media or rewrite playback history. Keep this rule linked to `OPERATIONS.md`. This was added after a private production incident where abandoned WebTorrent workers exhausted host RAM/swap and made unrelated services appear wedged.
- **webtorrent `-s` FIXED** (our PR #3011, fixes #331) — piece verification bug in `_markUnverified` re-selected ALL pieces, downloading entire torrent despite `-s`. Fix: `Selections.contains()` guard prevents re-selecting deselected pieces. Patched in-place on Darwin at `~/.local/share/mise/installs/node/24.14.0/lib/node_modules/webtorrent-cli/node_modules/webtorrent/lib/`. Verified: 27-file season pack → only target file + 1.7MB boundary pieces downloaded (sparse files, no actual disk waste). Smart ranking still prefers single-file torrents as belt-and-suspenders
- **Chromecast fetches URLs itself** — never hand it `localhost`. Use the configured stream host (`stream_host`) as the source of truth for Cast-fetchable URLs; it can be a DNS name when Cast devices resolve local DNS correctly, or an IP if they do not. Do not hard-code host rewrites after the cast call, because that only changes the client response and does not protect the Chromecast.
- **Local bypass completion markers require byte evidence** — `.spela_done` should only be written when search-result size metadata is available and the physical file bytes match it. Duration is not a byte size. See `OPERATIONS.md`.
- **Transcoded streaming via axum endpoint** (Mar 20, 2026) — `python3 -m http.server` sent `Content-Length` for a growing fMP4 file → Chromecast read that many bytes, thought stream complete, stopped after ~10s. Fix: `/stream/transcode` axum endpoint with chunked transfer (no Content-Length) + `StreamType::Live` (tells Chromecast not to expect fixed length). **5MB pre-buffer** proves sustained torrent+transcode health before casting. ffmpeg PID tracked for stream tailing + cleanup
- **EAC3/AC3/DTS → AAC transcode** — ffprobe auto-detect, ffmpeg with `-re` flag (real-time pacing, never outruns download) + `-reconnect_at_eof` (handles stalls). Fragmented MP4 output (`frag_keyframe+empty_moov`) playable from first byte
- **Subtitles burned into video** — when transcoding is needed, SRT subtitles are hardcoded via `-vf subtitles=` with NVENC GPU encoding (`h264_nvenc`). Works around rust_cast's lack of Cast protocol track support. Subtitles fetched from Stremio OpenSubtitles v3 (zero auth)
- **Self-healing: dead seeds** — if 0% download progress after 12s, auto-tries next search result (up to 3 retries). Cleanup between retries (kill ffmpeg, delete partial transcoded files)
- **catt mDNS ~40% flaky** — V2 uses rust_cast native + IP cache, no Python deps
- **webtorrent --chromecast broken** — serve via HTTP (:8888) + cast URL via rust_cast
- **systemd PATH** — needs explicit PATH env for mise shims (webtorrent not in default PATH)
- **HEVC/VP9/AV1 auto-transcode** (Mar 26) — `detect_codecs()` returns video+audio+duration. HEVC torrents auto-transcoded to H.264 via NVENC. Search ranking deprioritizes HEVC (but well-seeded HEVC beats dead H.264, threshold ≥5 seeds)
- **Intro clip** (Mar 21) — `~/.config/spela/intro.mp4` prepended via ffmpeg concat filter. Always plays when present (triggers NVENC pipeline). Both streams scaled to 1080p. 45s pre-buffer (vs 25s without intro). `--no-intro` to disable. **Gotcha**: `-dn -map_metadata -1` required — concat produces `bin_data` streams Chromecast rejects
- **Pause/resume works** (Mar 26) — no stall timeout (removed). Tested up to 10-minute pauses. ffmpeg exit is the only stream termination signal
- **Silent Range requests** (Mar 26) — `/stream/transcode` honors `Range: bytes=N-` by seeking the file, but ALWAYS returns 200 (never 206/Accept-Ranges). Chromecast switches to VOD mode if it sees range headers → disconnects on growing files. Silent approach: seek works, Chromecast stays in live mode
- **Seeking does NOT work** with Default Media Receiver — fMP4 has no byte-offset index, Chromecast can't map timestamps to file positions. Both `StreamType::Live` and `StreamType::Buffered` (with known duration from ffprobe) tested — seek always resets to start. Jellyfin solves this with Custom Receiver + Shaka Player + server-side seek-restart. **Next step**: $5 Google Cast SDK → Custom Receiver App
- **Post-playback reaper** (Mar 21) — background task monitors ffmpeg PID. When movie ends (ffmpeg exits), waits 3 min grace, kills webtorrent (~1.5GB freed), cleans files. Detects stream replacement and exits cleanly
- **GPU coexistence** — NVENC transcode (163MB), llama.cpp embeddings (2.8GB), Chrome kiosk (103MB) all fit in 4GB VRAM simultaneously
- **Jellyfin evaluated, rejected** (Mar 26) — library-centric (no "stream torrent NOW" flow), C#/.NET plugins, mixed Chromecast reliability. spela Custom Receiver is cleaner path for seeking
- **Custom Cast Receiver built** (Mar 26) — `static/cast-receiver.html` (Shaka Player + CAF v3). Self-configures via `/api/cast-config` because **rust_cast's Media struct only has 5 fields** (contentId, streamType, contentType, metadata, duration) — no tracks, customData, or textTrackStyle. Server endpoints: `/cast-receiver.html`, `/cast-receiver/intro.mp4`, `/cast-receiver/subs.vtt`, `/api/cast-config`, `/api/seek-restart`, `/api/position`, `/api/retry`. Blocked on $5 Cast SDK registration. Spec at `.claude/specs/custom-cast-receiver/`
- **Cast SDK terms** (Mar 26) — reviewed, no restrictions on content type or personal/commercial use. ToS governs SDK usage (user-initiated casting, no persistent receiver storage), not content. Safe for distribution
- **ffmpeg zombie fix** (Mar 26) — `std::mem::forget(child)` prevented `waitpid()`, creating zombie processes. Fix: `tokio::spawn(child.wait())` reaps immediately on exit
- **Torrentio sources** — aggregates 24 torrent sites, returns 76+ results per movie. Default providers = ALL providers (tested, same results). Other Stremio addons (MediaFusion, Knightcrawler, Comet) require encrypted config URLs or debrid service — not usable without self-hosting. Future: self-hosted Jackett on Darwin for private trackers
- **Movie disambiguation** — TMDB returns sequel over original for franchise names ("28 Years Later" → The Bone Temple). TODO: add `--year` flag to filter by release year
- **AdGuard blocks payments** (Mar 26) — `ogads-pa.clients6.google.com` caught by ad filters. 35 whitelist rules added for Google Payments, 3DS, Swedish banks
- **`do_cleanup` mid-flight kill** (Apr 15) — `do_play` had a misplaced `do_cleanup(&state)` between `start_webtorrent` and the transcode/cast step. `do_cleanup → stop_by_pid_file → kill_all_webtorrent()` SIGTERM'd its own just-started worker every time, so ffmpeg always hit `Connection refused` at `darwin.home:8888`. Removed in `4d3ef73`. Pre-start cleanup at the top of `do_play` (lines 387-397) was already sufficient. **NEVER reintroduce `do_cleanup` between worker spawn and reaper spawn.** Audit notes in `OPERATIONS.md` defense plan
- **Cast-failure cleanup defense** (Apr 15) — pre-existing leak: `do_play` returned errors from `cast_url()` failures without cleaning up the freshly-started workers, leaving them as orphans until next play / `kill-workers` / restart. Layer-8 defense added in `8735ea4`: `do_cleanup(&state)` on both cast-failure return branches. The reaper hasn't been spawned yet at that point, so explicit cleanup is the only thing standing between a cast hiccup and the Apr 8 incident class
- **`dir_size` is sparse-aware** (Apr 15) — webtorrent `-s <idx>` creates sparse placeholder files for unselected files in a torrent: `metadata.len()` reports the full multi-GB torrent size while only a few KB actually exist on disk. The buggy `dir_size` summed `len()` and tripped the 10 GB cap before any real download. Fix in `9f58307`: `metadata.blocks() * 512` via `std::os::unix::fs::MetadataExt`. Regression test `test_dir_size_counts_sparse_file_as_allocated_not_logical` pins it. Local Bypass already had its own sparse check (physical < logical → reject), only `dir_size` was missing it
- **Host filesystem safety floor** (Apr 15) — independent secondary cap protecting Darwin's router/runtime/database services from a runaway torrent. spela refuses new plays when `df -Pk ~/media/` reports <20 GB free, even when its own 10 GB media cap is satisfied. Best-effort (None on `df` failure) so a parser regression can't make spela unusable. `disk.rs::check_space`, commit `688bab5`. Complements rather than replaces the internal media cap
- **Mac CLI XDG path hardcoded** (Apr 15) — `dirs::config_dir()` returns `~/Library/Application Support` on macOS, hiding the Mac Mini's real `~/.config/spela/config.toml` and silently dropping the CLI back to `server = "localhost:7890"`. Hardcoded `dirs::home_dir().join(".config").join("spela").join("config.toml")` in `config.rs::config_path`, commit `267792f`. Regression test `test_config_path_uses_xdg_on_all_platforms` pins it. Same gotcha applies to any cross-platform Rust app using the `dirs` crate
- **Dolby Vision HEVC is NVENC-broken on GTX 1650** (Apr 15) — DV profile 5/7 RPU NAL units fail to parse. ffmpeg spams `Error parsing DOVI NAL unit` / `RPU validation failed: 0 <= el_bit_depth_minus8 = 32 <= 8` on every frame and the live transcode collapses to ~0.937x realtime (unviable for streaming). Search ranker now demotes DV titles below their non-DV siblings (4th tier). `has_dolby_vision_in_title()` is token-based (split on non-alphanumerics) to avoid matching DVD/DVDRip/DVD9. Commit `8313625`. Until/unless we add a `--strip-dolby-vision` ffmpeg pre-pass, DV titles are functionally incompatible
- **Ruby's `run_spela` defaults to `play 1`** (Apr 15) — Gemini was inventing the result number from her own preference for shiny labels, picking 2160p HDR DV HEVC as result #2 because "4K HDR" sounded better. Both layers of the prompt (`SPELA_TOOL_DECLARATION` in `~/.claude/hooks/conversation_engine.py` and the system tool-usage block) now say "default to args='1' — spela's ranker already returned its top pick, only override when the user explicitly asked for a specific quality/resolution/source". Dotfiles commit `916b179`. Spela's authority on which torrent to play, Ruby's job is to pass it through

## Chromecast Devices

Hardcoded fallback IPs in `src/cast.rs`:
- Fredriks TV: 192.168.4.126
- Vardagsrum: 192.168.4.58

DNS: `darwin.home` → darwin.home (AdGuard Home rewrite, configured Mar 18)
