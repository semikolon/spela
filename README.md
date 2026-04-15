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
- **Smart torrent ranking** -- 4 tiers: single-file > H.264 > non-Dolby-Vision > well-seeded. Dolby Vision profile 5/7 is demoted because NVENC can't parse RPU NAL units cleanly
- **Chromecast native** -- rust_cast + mDNS discovery, no Python dependencies
- **Transparent transcoding** -- HEVC to H.264, AC3/DTS to AAC, all via NVENC on your GPU. Output is HLS (MPEG-TS segments) which Chromecast's Default Media Receiver plays natively
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
/hls/master.m3u8 → /hls/playlist.m3u8 → /hls/seg_NNNNN.ts (HLS with MPEG-TS segments)
        ↓
Chromecast (rust_cast, StreamType::Buffered, mDNS discovery)
```

The CLI is a thin HTTP client. The server does everything. Run both on the same machine (laptop, desktop) or split them across your LAN. No dedicated server required -- `spela server` in one terminal tab, `spela play` in another.

## Worker Safety

spela's most important operational rule is that WebTorrent and ffmpeg are owned workers, not disposable background noise. Worker cleanup and media cleanup must stay separate:

- emergency cleanup should terminate WebTorrent/ffmpeg workers only;
- playback cleanup may remove temporary transcode files, but only through explicit playback paths;
- startup should reconcile stale workers from previous sessions;
- production deployments should add systemd/cgroup limits so a media worker cannot exhaust the host.

Run `spela kill-workers` on the media host for emergency worker-only cleanup. It sends `SIGTERM` to local WebTorrent workers and Spela-owned ffmpeg transcode workers; it does not remove media files or update playback history.

See [OPERATIONS.md](OPERATIONS.md) for the defense-in-depth plan and emergency checklist.

## Install

### Prerequisites

- **Rust** (for building spela)
- **webtorrent-cli** (`npm install -g webtorrent-cli`)
- **ffmpeg** (NVENC GPU transcoding if available, CPU fallback works fine — just slower startup)
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
server = "media.local:7890"
default_device = "Living Room TV"
subtitles = "en"
quality = "1080p"
tmdb_api_key = "your-key-here"
stream_host = "media.local"     # hostname or IP Chromecast can fetch from; never localhost
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

Install `systemd/spela-resource-limits.conf` as a drop-in at
`/etc/systemd/system/spela.service.d/resource-limits.conf` on production hosts.
That keeps media worker memory, swap, task count, and CPU use bounded without
putting host-specific secrets in the tracked unit file.

Completion markers (`.spela_done`) are only written after the downloaded file's
physical bytes match the expected torrent result size. Do not mark files complete
from playback duration alone; that can make partial files eligible for local
bypass.

## How Transcoding Works

Most torrents are H.264 + AC3/DTS audio. Chromecast only speaks H.264 + AAC. spela handles this transparently:

1. **ffprobe** detects audio and video codecs
2. If audio is AC3/EAC3/DTS → transcode to AAC
3. If video is HEVC/VP9/AV1 → transcode to H.264 via NVENC
4. If subtitles requested → burn into video via NVENC
5. If intro clip present → prepend via ffmpeg concat filter
6. **Output: HLS** — ffmpeg writes a `playlist.m3u8` + `seg_NNNNN.ts` MPEG-TS segments to `media/transcoded_hls/`
7. spela serves a synthetic master playlist (`/hls/master.m3u8`) with hardcoded `CODECS="avc1.640028,mp4a.40.2"` + `BANDWIDTH` + `RESOLUTION` because older Chromecast firmware (CrKey 1.56) refuses to load a bare media playlist without those hints
8. Pre-buffer waits for the manifest + first segment, then casts

Transcoding uses NVENC (GPU) when available. Without a GPU, ffmpeg falls back to CPU encoding (`libx264`) -- works fine, just takes ~30-60s to buffer instead of ~10s. Most torrents are H.264 + AC3 which only needs audio transcoding (instant, CPU-only).

The HLS muxer stays at v3/v4 — `-hls_playlist_type event` and `-hls_flags independent_segments` are both avoided because they bump the manifest to HLS v6, which Shaka Player on CrKey 1.56 can't parse. Segments are MPEG-TS, not fmp4, because rust_cast's `Media` struct doesn't expose `hlsSegmentFormat` (required for fmp4 on Default Media Receiver). See [TODO.md "Cast Pipeline Rework"](TODO.md) for the full history and the 10 compounding failure modes that got us here.

## Known Limitations

- **Seeking/rewind** is limited. Live HLS (while ffmpeg is still writing segments) can't seek past the current segment. Once ffmpeg finishes and the `#EXT-X-ENDLIST` tag appears, Chromecast can seek to any segment boundary. A Custom Cast Receiver App (Google Cast SDK, $5 registration) would enable server-side seek-restart for on-the-fly seeking during transcode. This is planned.
- **Chromecast DNS** -- Chromecast devices hardcode Google DNS (8.8.8.8 / 8.8.4.4) and ignore the DHCP-advertised LAN resolver. If `stream_host` is a hostname (`media.local`), the receiver can't resolve it and the LOAD silently fails. Either use a LAN IP for `stream_host`, or install a router-side DNAT hijack that forces the Chromecast's port-53 traffic through your local resolver. See [OPERATIONS.md](OPERATIONS.md) and [CLAUDE.md](CLAUDE.md).
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
| `/hls/master.m3u8` | GET | Synthetic HLS master playlist (with CODECS/BANDWIDTH/RESOLUTION hints) |
| `/hls/playlist.m3u8` | GET | Media playlist written by ffmpeg (growing during transcode) |
| `/hls/{segment}` | GET | MPEG-TS segment (`seg_NNNNN.ts`), supports Range requests |
| `/cast-info` | POST | Chromecast playback details |

## Tests

```bash
cargo test
```

## License

MIT
