# spela — AI-Agent-Ready Media Controller

## Overview

Rust CLI + HTTP API server for torrent-to-Chromecast streaming. Single 6.9MB binary, zero runtime deps on target (except webtorrent-cli and ffmpeg).

**Status**: v2.0.0 deployed on Darwin as systemd service. First successful cast: Legion S01E05 → Fredriks TV (Mar 18, 2026).

**Architecture**: `spela server` on Darwin (`darwin.home:7890`). `spela <command>` thin HTTP client from any LAN machine. Voice assistants call HTTP API directly.

## Usage

```bash
spela search "legion" --season 1 --episode 5   # Search + rank results
spela play 1                                     # Play result #1 (auto: magnet, file_index, metadata)
spela play 1 --cast "Vardagsrum"                 # Play to living room TV
spela next                                       # Next episode
spela pause / resume / stop                      # Playback control
spela status / history / targets                 # Info
```

`spela play <N>` is the primary flow. Carries all metadata (file_index, IMDB, show, season, episode) automatically from the last search.

## Key Files

- `src/main.rs` — CLI (clap) + server startup. Detects `play 1` (result ID) vs `play magnet:...`
- `src/server.rs` — axum HTTP API, 14 endpoints. Orchestrates search→play→cast pipeline
- `src/search.rs` — TMDB + Torrentio. **Smart ranking**: single-file torrents above season packs, then by seeds
- `src/cast.rs` — Native Chromecast via rust_cast + mdns-sd. 3x retry + IP cache + known device fallback
- `src/torrent.rs` — webtorrent-cli subprocess management with `-s <fileIdx>` for file selection
- `src/transcode.rs` — ffprobe codec detection + ffmpeg EAC3/DTS/AC3→AAC transcode
- `src/subtitles.rs` — Stremio OpenSubtitles v3 (zero auth), SRT→WebVTT
- `src/disk.rs` — 5GB cap, 24h file cleanup
- `src/state.rs` — state.json + last_search.json (play-by-id)
- `src/config.rs` — ~/.config/spela/config.toml

## Build & Deploy

```bash
# Build on Darwin directly (has Rust 1.94):
ssh darwin 'cd ~/spela && cargo build --release'
ssh darwin 'sudo systemctl stop spela && cp ~/spela/target/release/spela ~/.local/bin/ && sudo systemctl start spela'

# Or sync source then build:
rsync -av --exclude target ~/Projects/spela/src/ darwin:~/spela/src/
```

## Darwin Setup

- **Binary**: `~/.local/bin/spela`
- **Service**: `/etc/systemd/system/spela.service` (auto-start, restart on crash)
- **Config**: `~/.config/spela/config.toml` (TMDB key, default device, LAN IP)
- **State**: `~/.spela/` (state.json, last_search.json, devices.json, webtorrent.log)
- **Media**: `~/media/` (temporary, 10GB cap, 24h auto-cleanup)
- **Deps**: webtorrent-cli (mise/npm), ffmpeg (apt)
- **Firewall**: ports 8888 (webtorrent HTTP) + 7890 (spela API) open to LAN in nftables
- **PATH**: systemd service needs mise shims path for webtorrent

## Hard-Won Lessons

- **webtorrent `-s` BROKEN for packs** (GitHub #331, unmerged PR #2981) — piece verification bug marks ALL pieces as unverified → downloads entire torrent despite `-s`. Not piece-boundary overlap — full regression since v5.1.3. **Fix**: smart ranking puts single-file torrents (file_index=0) first. Torrentio `fileIdx` IS 0-based and maps directly to webtorrent's `-s` index. If only packs available, `-s` still gives the selected file streaming priority (plays first) but everything downloads in background
- **localhost doesn't work for Chromecast** — always use `192.168.4.1`, Chromecast fetches URL itself
- **EAC3/AC3/DTS silent** — auto-detect with ffprobe, progressive transcode to AAC stereo
- **catt mDNS ~40% flaky** — V2 uses rust_cast native + IP cache, no Python deps
- **webtorrent --chromecast broken** — serve via HTTP (:8888) + cast URL via rust_cast
- **Subtitle tracks** — rust_cast doesn't support Cast protocol tracks yet. VTT conversion ready, track injection TODO
- **systemd PATH** — needs explicit PATH env for mise shims (webtorrent not in default PATH)

## Chromecast Devices

Hardcoded fallback IPs in `src/cast.rs`:
- Fredriks TV: 192.168.4.126
- Vardagsrum: 192.168.4.58

DNS: `darwin.home` → 192.168.4.1 (AdGuard Home rewrite, configured Mar 18)
