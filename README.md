# spela

**Search. Play. Done.** Torrent-to-Chromecast in one command.

```bash
spela search "28 Years Later"          # finds the movie, ranks results
spela play 1 --cast "Living Room TV"   # streams to your TV with subtitles
spela pause                            # go grab a snack
spela resume                           # back to zombies
```

spela is a single Rust binary that searches for torrents, streams them via webtorrent, transcodes incompatible audio/video on the fly with ffmpeg, fetches subtitles, and casts to your Chromecast. No media library to maintain. No 15-app Docker stack. Just say what you want to watch.

## Why?

Because "I want to watch a movie" shouldn't require Sonarr + Radarr + Prowlarr + qBittorrent + Jellyfin + a NAS + a weekend of configuration. And because your voice assistant should be able to play something when you ask nicely.

## Features

- **Instant torrent streaming** -- search by name, play by number. No downloads, no waiting
- **Auto-detect movie vs TV** -- TMDB figures out what you're searching for
- **Smart torrent ranking** -- prefers single-file over season packs, H.264 over HEVC, well-seeded over dead
- **Chromecast native** -- rust_cast + mDNS discovery, no Python dependencies
- **Transparent transcoding** -- HEVC to H.264, AC3/DTS to AAC, all via NVENC on your GPU. You don't notice, it just works
- **Subtitles** -- auto-fetched from OpenSubtitles, burned into the stream
- **Intro clip** -- your own Netflix-style bumper before every stream (drop an `intro.mp4` in config)
- **Pause/resume** -- tested up to 10-minute pauses. No timeouts, no dropped connections
- **Post-playback cleanup** -- kills webtorrent and cleans temp files after the movie ends
- **Voice-ready** -- works with voice assistants via CLI or HTTP API. Our assistant Ruby (Gemini + MCP) uses `spela search` and `spela play` directly
- **Self-healing** -- dead torrents auto-retry the next result. Failed casts retry 3x with backoff

## Architecture

```
You → spela CLI (thin HTTP client)
        ↓
spela server (axum, runs on your LAN)
        ↓
TMDB (metadata) → Torrentio (torrents) → webtorrent-cli (download + HTTP serve)
        ↓
ffmpeg (transcode if needed: HEVC→H.264, AC3→AAC, subtitle burn-in, intro concat)
        ↓
/stream/transcode (chunked HTTP, growing file, tails ffmpeg output)
        ↓
Chromecast (rust_cast, StreamType::Live, mDNS discovery)
```

The CLI is a thin HTTP client. The server does everything. This means any device on your LAN can control playback -- phones, voice assistants, scripts, cron jobs.

## Install

### Prerequisites

- **Rust** (for building spela)
- **webtorrent-cli** (`npm install -g webtorrent-cli`)
- **ffmpeg** with NVENC support (for GPU transcoding) or CPU fallback
- A **TMDB API key** (free at [themoviedb.org](https://www.themoviedb.org/settings/api))
- A **Chromecast** on the same network

### Build

```bash
git clone https://github.com/semikolon/spela.git
cd spela
cargo build --release
```

### Setup

```bash
./target/release/spela setup
```

This discovers your Chromecast devices, asks you to pick a default, and saves config to `~/.config/spela/config.toml`.

### Run the server

```bash
spela server
# or as a systemd service — see below
```

## Usage

```bash
# Search (auto-detects movie vs TV show)
spela search "Severance"
spela search "Legion" --season 1 --episode 5

# Play (result number from last search)
spela play 1                             # cast to default device
spela play 1 --cast "Bedroom TV"         # cast to specific device
spela play 1 --no-subs                   # skip subtitles
spela play 1 --no-intro                  # skip intro clip

# Playback controls
spela pause
spela resume
spela stop
spela volume 80
spela seek 300                           # seek to 5:00 (requires Custom Receiver, see below)

# Navigation
spela next                               # next episode (auto-search + play)
spela prev                               # previous episode

# Info
spela status                             # what's playing
spela targets                            # discover Chromecast devices
spela history                            # recently played
spela config                             # show preferences
spela config default_device "My TV"      # set default Chromecast
```

## Configuration

Config lives at `~/.config/spela/config.toml`:

```toml
server = "localhost:7890"
default_device = "Living Room TV"
subtitles = "en"
quality = "1080p"
tmdb_api_key = "your-key-here"
lan_ip = "192.168.1.100"        # your server's LAN IP (Chromecast fetches from this)
media_dir = "~/media"
port = 7890

# Fallback IPs for when mDNS discovery fails (optional)
[known_devices]
"Living Room TV" = "192.168.1.50"
"Bedroom TV" = "192.168.1.51"
```

## Intro Clip

Drop a short MP4 at `~/.config/spela/intro.mp4` and it plays before every stream -- your own Netflix-style ident. 1080p H.264+AAC recommended, 3-5 seconds. We made ours with [Kling AI](https://klingai.com).

## Systemd Service

```ini
[Unit]
Description=spela media controller
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=youruser
ExecStart=/home/youruser/.local/bin/spela server
Restart=on-failure
RestartSec=5
Environment=TMDB_API_KEY=your-key-here

[Install]
WantedBy=multi-user.target
```

## How Transcoding Works

Most torrents are H.264 + AC3/DTS audio. Chromecast only speaks H.264 + AAC. spela handles this transparently:

1. **ffprobe** detects audio and video codecs
2. If audio is AC3/EAC3/DTS → transcode to AAC
3. If video is HEVC/VP9/AV1 → transcode to H.264 via NVENC
4. If subtitles requested → burn into video via NVENC
5. If intro clip present → prepend via ffmpeg concat filter
6. Output: fragmented MP4, streamed via chunked HTTP as it's being written
7. 5MB pre-buffer before casting (proves the torrent is healthy)

All transcoding uses NVENC (GPU) when available. CPU fallback works but is slower.

## Known Limitations

- **Seeking/rewind** doesn't work with the Default Media Receiver. The Chromecast can't seek in a growing fMP4 stream. A Custom Cast Receiver App (Google Cast SDK, $5 registration) would enable seeking via server-side seek-restart. This is planned.
- **Intro clip** triggers full NVENC re-encoding (both intro and main stream). This adds ~30s to startup but is invisible during playback.
- **Google Home pause** works but the stream must stay connected. Very long pauses (hours) may drop the TCP connection.

## API

Every CLI command maps to an HTTP endpoint:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/search?q=name` | GET | Search for content |
| `/play` | POST | Start playback |
| `/stop` | POST | Stop playback |
| `/status` | GET | Current playback info |
| `/pause` | POST | Pause |
| `/resume` | POST | Resume |
| `/seek` | POST | Seek to position |
| `/volume` | POST | Set volume |
| `/next` | POST | Next episode |
| `/prev` | POST | Previous episode |
| `/targets` | GET | Discover Chromecast devices |
| `/history` | GET | Watch history |
| `/config` | GET/POST | Preferences |
| `/stream/transcode` | GET | Transcoded media stream |
| `/cast-info` | POST | Chromecast playback details |

## Tests

```bash
cargo test
```

## License

MIT
