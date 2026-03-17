# spela — AI-Agent-Ready Media Controller

## Overview

Rust CLI + HTTP API server for torrent-to-Chromecast streaming. Single binary, zero runtime deps on target machine (except webtorrent-cli and ffmpeg).

**Architecture**: `spela server` runs on Darwin (Dell Optiplex, `192.168.4.1:7890`). `spela <command>` is a thin HTTP client run from any machine. Voice assistants and AI agents use the HTTP API directly.

## Key Files

- `src/main.rs` — CLI (clap) + server startup routing
- `src/server.rs` — axum HTTP API, all endpoint handlers, orchestrates play pipeline
- `src/search.rs` — TMDB + Torrentio API (search → metadata → magnet links)
- `src/cast.rs` — Chromecast control (rust_cast + mdns-sd), retry + IP cache
- `src/torrent.rs` — webtorrent-cli subprocess management
- `src/transcode.rs` — ffprobe codec detection + ffmpeg AAC transcode
- `src/subtitles.rs` — Stremio OpenSubtitles v3 (zero auth), SRT→VTT
- `src/disk.rs` — 5GB cap watchdog, 24h file cleanup
- `src/state.rs` — ~/.spela/state.json (current stream, history, preferences)
- `src/config.rs` — ~/.config/spela/config.toml parsing

## Build & Deploy

```bash
cargo build --release                    # Mac (arm64)
scp target/release/spela darwin:~/.local/bin/  # Deploy to Darwin
# OR cross-compile for x86_64 Linux:
cargo build --release --target x86_64-unknown-linux-gnu
```

## Darwin Dependencies

Only `webtorrent-cli` (npm -g) and `ffmpeg` (apt) needed on Darwin. Everything else is in the binary.

## Hard-Won Lessons (from V1)

- **localhost doesn't work for Chromecast** — always use `192.168.4.1` (Darwin LAN IP)
- **catt mDNS ~40% flaky** — V2 uses rust_cast + IP cache, 3x retry
- **webtorrent --chromecast broken** — serve via HTTP (:8888) + cast URL via rust_cast
- **AC3/DTS silent on Chromecast** — auto-detect with ffprobe, transcode to AAC
- **Darwin firewall blocks ports** — 8888/7890 open to LAN in nftables
- **Season packs** — use `-s <fileIdx>`, prefer single-file torrents
- **Subtitle tracks** — rust_cast doesn't support Cast protocol tracks yet. VTT served via HTTP, track injection is TODO

## Chromecast Devices

- Fredriks TV: 192.168.4.126
- Vardagsrum: 192.168.4.58
- Hardcoded fallback IPs in `src/cast.rs` KNOWN_DEVICES

## Config

`~/.config/spela/config.toml` — see `config.toml.example`. TMDB_API_KEY also reads from env.

## State

`~/.spela/state.json` — current stream, watch history (50 entries), preferences.
`~/.spela/devices.json` — cached Chromecast IPs from mDNS discovery.
