# spela — AI-Agent-Ready Media Controller

## Overview

Rust CLI + HTTP API server for torrent-to-Chromecast streaming. Single 6.9MB binary, zero runtime deps on target (except webtorrent-cli and ffmpeg).

**Status**: v2.0.0 deployed on Darwin as systemd service. Chromecast casting stable (Mar 20 streaming fix).

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
- `src/server.rs` — axum HTTP API, 14 endpoints. Orchestrates search→play→cast pipeline
- `src/search.rs` — TMDB + Torrentio. **Auto-detect movie vs TV** via TMDB multi-search. Smart ranking: single-file torrents above season packs, then by seeds
- `src/cast.rs` — Native Chromecast via rust_cast + mdns-sd. 3x retry + IP cache + known device fallback
- `src/torrent.rs` — webtorrent-cli subprocess management with `-s <fileIdx>` for file selection
- `src/transcode.rs` — ffprobe codec detection + ffmpeg EAC3/DTS/AC3→AAC transcode + intro concat
- `src/subtitles.rs` — Stremio OpenSubtitles v3 (zero auth), SRT→WebVTT
- `src/disk.rs` — 5GB cap, 24h file cleanup
- `src/state.rs` — state.json + last_search.json (play-by-id)
- `src/config.rs` — ~/.config/spela/config.toml

## Build & Deploy

```bash
# Repo cloned on Darwin at ~/Projects/spela (Rust 1.94):
cd ~/Projects/spela && git push   # from Mac
ssh darwin 'cd ~/Projects/spela && git pull && cargo build --release'
ssh darwin 'sudo systemctl stop spela && cp ~/Projects/spela/target/release/spela ~/.local/bin/ && sudo systemctl start spela'
```

## Darwin Setup

- **Binary**: `~/.local/bin/spela`
- **Service**: `/etc/systemd/system/spela.service` (auto-start, restart on crash)
- **Config**: `~/.config/spela/config.toml` (TMDB key, default device, LAN IP)
- **State**: `~/.spela/` (state.json, last_search.json, devices.json, webtorrent.log)
- **Intro**: `~/.config/spela/intro.mp4` (5s Kling AI-generated bumper, 1080p H.264+AAC)
- **Media**: `~/media/` (temporary, 10GB cap, 24h auto-cleanup). Transcoded files cleaned on stop
- **Deps**: webtorrent-cli (mise/npm), ffmpeg (apt)
- **Firewall**: ports 8888 (webtorrent HTTP) + 7890 (spela API) open to LAN in nftables
- **PATH**: systemd service needs mise shims path for webtorrent

## Hard-Won Lessons

- **webtorrent `-s` FIXED** (our PR #3011, fixes #331) — piece verification bug in `_markUnverified` re-selected ALL pieces, downloading entire torrent despite `-s`. Fix: `Selections.contains()` guard prevents re-selecting deselected pieces. Patched in-place on Darwin at `~/.local/share/mise/installs/node/24.14.0/lib/node_modules/webtorrent-cli/node_modules/webtorrent/lib/`. Verified: 27-file season pack → only target file + 1.7MB boundary pieces downloaded (sparse files, no actual disk waste). Smart ranking still prefers single-file torrents as belt-and-suspenders
- **localhost doesn't work for Chromecast** — always use `192.168.4.1`, Chromecast fetches URL itself
- **Transcoded streaming via axum endpoint** (Mar 20, 2026) — `python3 -m http.server` sent `Content-Length` for a growing fMP4 file → Chromecast read that many bytes, thought stream complete, stopped after ~10s. Fix: `/stream/transcode` axum endpoint with chunked transfer (no Content-Length) + `StreamType::Live` (tells Chromecast not to expect fixed length). **5MB pre-buffer** proves sustained torrent+transcode health before casting. ffmpeg PID tracked for stream tailing + cleanup
- **EAC3/AC3/DTS → AAC transcode** — ffprobe auto-detect, ffmpeg with `-re` flag (real-time pacing, never outruns download) + `-reconnect_at_eof` (handles stalls). Fragmented MP4 output (`frag_keyframe+empty_moov`) playable from first byte
- **Subtitles burned into video** — when transcoding is needed, SRT subtitles are hardcoded via `-vf subtitles=` with NVENC GPU encoding (`h264_nvenc`). Works around rust_cast's lack of Cast protocol track support. Subtitles fetched from Stremio OpenSubtitles v3 (zero auth)
- **Self-healing: dead seeds** — if 0% download progress after 12s, auto-tries next search result (up to 3 retries). Cleanup between retries (kill ffmpeg, delete partial transcoded files)
- **catt mDNS ~40% flaky** — V2 uses rust_cast native + IP cache, no Python deps
- **webtorrent --chromecast broken** — serve via HTTP (:8888) + cast URL via rust_cast
- **systemd PATH** — needs explicit PATH env for mise shims (webtorrent not in default PATH)
- **Intro clip** (Mar 21, 2026) — `~/.config/spela/intro.mp4` prepended via ffmpeg concat filter when transcoding is active. Both streams scaled to 1080p, NVENC re-encodes the combined output. 45s pre-buffer (vs 25s without intro) due to heavier pipeline. `--no-intro` to disable. Only activates when audio transcode is already needed; direct-cast skips intro. **Gotcha**: strip `-dn -map_metadata -1` required — concat produces `bin_data` streams that Chromecast rejects
- **Chromecast progress bar during intro** — Default Media Receiver (CC1AD845) always shows overlay for ~5s on media load. Custom Receiver App ($5 Google Cast SDK registration, ~50-line HTML) would give full UI control. Not yet implemented
- **GPU coexistence** — NVENC transcode (163MB), llama.cpp embeddings (2.8GB), Chrome kiosk (103MB) all fit in 4GB VRAM simultaneously

## Chromecast Devices

Hardcoded fallback IPs in `src/cast.rs`:
- Fredriks TV: 192.168.4.126
- Vardagsrum: 192.168.4.58

DNS: `darwin.home` → 192.168.4.1 (AdGuard Home rewrite, configured Mar 18)
