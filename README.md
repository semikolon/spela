# spela

**Search. Play. Done.** Torrent-to-Chromecast in one command.

```bash
spela search "28 Years Later"          # finds the movie, ranks results
spela play 1 --cast "Living Room TV"   # streams to your TV with subtitles
spela pause                            # go grab a snack
spela resume                           # back to zombies
```

spela is a single Rust binary that searches for torrents, streams them through an embedded BitTorrent client (librqbit), transcodes incompatible audio/video on the fly with ffmpeg, fetches subtitles, and casts to your Chromecast. No media library to maintain. No 15-app Docker stack. No Node subprocess. Just say what you want to watch.

## Why?

Because "I want to watch a movie" shouldn't require Sonarr + Radarr + Prowlarr + qBittorrent + Jellyfin + a NAS + a weekend of configuration. And because your voice assistant should be able to play something when you ask nicely.

## Features

- **Instant torrent streaming** -- search by name, play by number. No downloads, no waiting
- **Auto-detect movie vs TV** -- TMDB figures out what you're searching for
- **Smart torrent ranking** -- 5 tiers tuned for 1080p TVs: single-file > non-Dolby-Vision (HARD GPU gate, RPU NAL parsing fails on consumer NVENC) > target resolution (1080p > 720p > 480p > 2160p; 4K demoted below 480p because 1080p TVs can't display the extra pixels) > H.264 > more seeds. **Seed-disparity override**: when a HEVC alternative has ≥30× the seeds of the H.264 tier-4 winner, the HEVC wins — NVENC absorbs HEVC→H.264 transcode in 5-10 s, whereas a starved swarm can block forever
- **Chromecast native** -- rust_cast + mDNS discovery, no Python dependencies
- **Transparent transcoding** -- HEVC to H.264, AC3/DTS to AAC, all via NVENC on your GPU. Output is HLS (MPEG-TS segments) which Chromecast's Default Media Receiver plays natively
- **Subtitles** -- auto-fetched from OpenSubtitles, burned into the stream
- **Intro clip** -- your own Netflix-style bumper before every stream (drop an `intro.mp4` in config)
- **Pause/resume** -- tested up to 10-minute pauses. No timeouts, no dropped connections. **v3.4.2 pause-gated auto-recast**: even when CrKey 1.56 receiver firmware unloads its app after long pauses (~16 min, transitions to IDLE), spela will NEVER auto-resume — it correctly distinguishes "user-paused + receiver app dormant" from "stream wedged, needs recovery"
- **Seek** -- `spela seek <pos>` jumps to absolute episode position via native cast.seek (instant within the current transcode window). `spela seek` (no arg) resumes from the saved HWM. Seeks before the current transcode's start point error with a hint to use `spela play --seek N` for a re-transcode
- **Post-playback cleanup** -- terminates the active torrent and any ffmpeg transcode workers, cleans temp files after playback ends
- **Voice-ready** -- works with voice assistants via CLI or HTTP API. Our assistant Ruby (Gemini + MCP) uses `spela search` and `spela play` directly
- **Self-healing** -- 20 s stream-start fail-fast detects starved swarms (zero HLS segments produced) and auto-retries with the next search result. Failed casts retry up to 3× with backoff. Head-of-stream probe rejects a play before LOAD if librqbit hasn't fetched piece 0 yet, so the Chromecast never sees a half-formed manifest

## Architecture

```
You → spela CLI (thin HTTP client)
        ↓
spela server (axum, runs on your LAN)
        ↓
TMDB (metadata) → Torrentio (torrents) → librqbit (embedded BitTorrent, in-process)
        ↓
/torrent/{id}/stream/{file_idx} (loopback-only Range-supporting HTTP route)
        ↓
ffmpeg (transcode if needed: HEVC→H.264, AC3→AAC, subtitle burn-in, intro concat)
        ↓
/hls/master.m3u8 → /hls/playlist.m3u8 → /hls/seg_NNNNN.ts (HLS with MPEG-TS segments)
        ↓
Chromecast (rust_cast, StreamType::Buffered, mDNS discovery)
```

The CLI is a thin HTTP client. The server does everything. Run both on the same machine (laptop, desktop) or split them across your LAN. No dedicated server required -- `spela server` in one terminal tab, `spela play` in another.

## Worker Safety

spela's most important operational rule is that ffmpeg transcode workers and librqbit's torrent session are owned resources, not disposable background noise. Worker cleanup and media cleanup must stay separate:

- emergency cleanup should terminate transcode workers only;
- playback cleanup may remove temporary transcode files, but only through explicit playback paths;
- startup should reconcile stale resources from previous sessions (librqbit torrents that outlived a crash, ffmpeg orphans, and — as a one-shot transition aid — webtorrent-cli processes left behind by pre-v3.3.0 installs);
- production deployments should add systemd/cgroup limits so a media worker cannot exhaust the host.

Run `spela kill-workers` on the media host for emergency worker-only cleanup. It sends `SIGTERM` to spela-owned ffmpeg transcode workers (and any orphan pre-v3.3.0 webtorrent-cli processes) without removing media files or updating playback history. The librqbit session is in-process, so a full `systemctl restart spela` is the equivalent operation for the torrent side.

See [OPERATIONS.md](OPERATIONS.md) for the defense-in-depth plan and emergency checklist.

## Install

### Prerequisites

- **Rust** (for building spela; the BitTorrent client is embedded, no Node dependency)
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
