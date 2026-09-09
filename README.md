# spela

**Search. Play. Done.** One binary between "I want to watch this" and it playing.

```bash
spela search "28 Years Later"     # finds it, ranks the sources
spela play 1                      # plays it
```

Or open `http://<host>:7890/remote` on a phone or laptop and tap. The web remote is the
main way spela is used now; the CLI and the HTTP API do the same things.

spela searches for torrents, streams them through an embedded BitTorrent client
(librqbit), transcodes what needs transcoding with ffmpeg, finds and syncs subtitles, and
plays to whatever screen you point it at: a Chromecast, VLC on a desktop, a browser tab,
or a local renderer on a small Linux box. It also keeps track of what you have watched
and what is worth watching next.

No media library to maintain. No fifteen-app Docker stack. No Node subprocess.

## Why?

Because "I want to watch a movie" shouldn't require Sonarr + Radarr + Prowlarr +
qBittorrent + Jellyfin + a NAS + a weekend of configuration. And because your voice
assistant should be able to play something when you ask nicely.

## What it does

**Playing things**

- **Search by name, play by number.** TMDB works out whether you mean a film or a series;
  `S03E07` in the query is understood.
- **Four kinds of screen.** Chromecast (transcoded to H.264 via NVENC), VLC on a desktop
  (native decode, the default there), a browser tab (HEVC passed through untouched where
  the browser can decode it), and a local renderer on a small Linux box. The remote
  defaults to whichever suits the device you opened it on.
- **A quality control, because the guards are judgement.** `Auto · 4K · 1080p · Saver`.
  Auto is the ranker's own opinion; the others exist because it is judgement rather than
  fact, and both directions of being wrong are ones only you can see. Saver caps at
  1080p **and** takes the smallest encode, which is roughly 12 GB against 1 GB for the
  same episode; it is the default off-LAN, where bandwidth is likely to cost money.
- **Subtitles that are actually in sync.** An embedded track in your language is used
  first, because it is synced by construction. Otherwise OpenSubtitles, aligned to the
  release with [alass](https://github.com/kaegi/alass) against the densest embedded
  track, and cached per episode so the second open is instant rather than forty seconds.
- **Never a dub.** The audio track is chosen by the title's original language, so a
  Danish film plays Danish and a Korean one Korean, even when the release labels the dub
  as default.
- **Local files first.** If a copy is already on disk it is served straight from there,
  fully seekable, with no torrent involved — including files a separate library daemon
  serves from an external drive, and a season pack opens an episode chooser rather than
  guessing.
- **Repeat plays start instantly.** A completed Chromecast transcode is kept in a
  size-capped cache, so watching the same thing again skips the transcode entirely.
- **The next episode is lined up inside the running VLC** as you approach the end, so it
  continues with no relaunch and no gap.

**Keeping track**

- **A queue that knows where you are.** Continue watching, new episodes of shows you
  follow, what airs next and when, and recommendations.
- **A watch ledger** as the single record of both progress and what you have seen, with
  loved / not-for-me ratings and a Rewatch shelf that reads it from the other side.
- **Air dates from TVmaze**, falling back to TMDB, because TMDB lags the real platform
  drop by about a day.
- **Enough to decide with**: a Rotten Tomatoes score, a click-to-load trailer, and for a
  series a status badge (returning, undecided, ended, cancelled) merged from two sources
  that disagree usefully — one of them has a word for "aired but undecided" and the other
  distinguishes cancelled from finished.
- **A dated intel note** under that badge, written by an assistant through `POST
  /show-notes` rather than fetched: reception, renewal talk, and why a thing was
  cancelled. It is pull rather than push, so a note is only as current as its last
  refresh, and the interface says so once it goes stale.

**Behaving itself**

- **Bounded disk.** A 100 GB cache cap with least-recently-used eviction, plus a
  host free-space floor, enforced on every path that can start a download.
- **Bounded upload.** Seeding is capped at 100 Mbit/s, and a finished download stops
  sharing once you have watched it. Sharing back is the default; sharing forever, for
  things nobody will watch, is not.
- **Downloads are remembered** across a restart, along with which pieces are already
  good, so resuming a source is a lookup rather than a swarm round-trip and a full
  re-read of the file.
- **It gives up on dead sources instead of spinning.** A source delivering nothing after
  a grace period is raced against alternatives of the same or better picture quality, and
  the remote rotates past dead ones out loud rather than silently.
- **Thin swarms get help**: twenty public trackers injected session-wide, an inbound peer
  listener so other peers can reach you, the head and tail of a file fetched first so a
  player's container probe does not block, and a blocklist that bans the decoy ranges
  which fill connection slots without ever sending bytes.
- **Redeploys do not drop the port.** The listening sockets are held by systemd across a
  restart, so a browser or VLC reconnects rather than failing.

## How sources are ranked

Lower is better, and each tier is a value computed per result — never a pairwise
threshold, which is how you get an ordering that changes depending on the input order.

1. **Non-Dolby-Vision first**, and this one is a hard gate for the Chromecast: consumer
   NVENC cannot parse a DV profile 5/7 RPU, so those releases produce no output at all.
   For native decoding it is a preference rather than a gate. Plain HDR10 is unaffected.
2. **Resolution, scoped to the target.** A Chromecast is a 1080p screen fed by a
   transcoder, so `1080p > 720p > 480p > 2160p`. A 4K monitor decoding natively is the
   other way round, `2160p > 1080p > 720p > 480p`, because a 1080p source upscaled twice
   reads as soft at close distance whatever its bitrate. The one thing that can still
   push a 4K down is bits-per-pixel: a 2160p carrying under half the best 1080p's data
   per pixel will look worse than the 1080p it would displace.
3. **Language fit** against the show's original language: a clean release, then a
   multi-market or dual-language one, then a foreign dub.
4. **H.264 over HEVC**, but only for targets that re-encode through NVENC, where H.264
   plays instantly. Skipped entirely for native decoding.
5. **Bitrate, via file size.** Every candidate in one search is the same minutes, so size
   *is* bitrate, in logarithmic bands.
6. **More seeds**, as the final tiebreak.

**Seeds prefer; they do not exclude.** A viability bar used to demote thin swarms
outright, which is a prediction about delivery in a system that measures delivery —
racing, a stall gate, and rotation past dead sources all observe what actually arrives.
**Being one episode inside a season pack costs nothing** either: spela selects the single
file, so a pack is not a larger download.

## Architecture

```
Web remote (/remote) · CLI · HTTP API · voice assistant
        ↓
spela server (axum, on your LAN)
        ↓
TMDB + TVmaze (metadata) → Torrentio (sources) → librqbit (embedded BitTorrent)
        ↓
a complete local file, if there is one — otherwise a Range-capable stream off the torrent
        ↓
ffmpeg, only where it is needed:
   Chromecast  → H.264 + AAC via NVENC, HLS with MPEG-TS segments, subtitles burned in
   browser     → HEVC copied into fMP4, full resolution and bitrate, no re-encode
   VLC / local → no transcode at all; the player decodes natively
```

The CLI is a thin HTTP client and the server does everything, so both can live on one
machine or be split across a LAN.

## Install

### Prerequisites

- **Rust** (the BitTorrent client is embedded — no Node dependency)
- **ffmpeg** (NVENC if you have it; CPU fallback works, just slower to start)
- A **TMDB API key** (free at [themoviedb.org](https://www.themoviedb.org/settings/api))
- Optionally **alass** for subtitle alignment, and a Chromecast or VLC

### Build and set up

```bash
git clone https://github.com/semikolon/spela.git
cd spela
cargo build --release
./target/release/spela setup     # discovers devices, writes ~/.config/spela/config.toml
spela server
```

Then open `http://<host>:7890/remote`.

## Usage

```bash
# Search — the media kind is explicit when you know it, guessed when you don't
spela search "Severance"
spela search "Legion" --season 1 --episode 5

# Play (result number from the last search)
spela play 1
spela play 1 --cast "Bedroom TV"
spela play 1 --no-subs
spela play 1 --seek 600            # start ten minutes in, re-transcoding from there

# Controls
spela pause / resume / stop
spela volume 80
spela seek 300                     # absolute position in the episode, not in the stream
spela seek                         # resume where you left off

# Navigation and info
spela next / prev
spela status / targets / history / config
spela config default_device "My TV"
```

## Configuration

`~/.config/spela/config.toml`:

```toml
server = "media.local:7890"
default_device = "Living Room TV"
subtitles = "en"
tmdb_api_key = "your-key-here"
mdblist_api_key = "optional-key"   # Rotten Tomatoes scores; omit and they are simply absent
hls_cache_cap_mb = 12288           # completed Chromecast transcodes, least-recently-used
stream_host = "media.local"      # what a Chromecast can fetch from; never localhost
media_dir = "~/media"
port = 7890

# Optional: fallbacks for when mDNS discovery fails
[known_devices]
"Living Room TV" = "192.168.1.50"
```

The server rewrites this file itself to record things it learns at runtime, so treat the
live copy as authoritative rather than restoring an old one over it.

## Worker safety

ffmpeg transcode workers and the torrent session are owned resources, not disposable
background noise, and worker cleanup must stay separate from media cleanup.
`spela kill-workers` terminates spela's own ffmpeg workers without touching media files or
playback history. See [OPERATIONS.md](OPERATIONS.md) for the emergency checklist and the
systemd resource limits to install on a production host.

A download is marked complete only when the file's physical bytes match the expected
size. Never infer completeness from playback duration — that makes partial files eligible
to be served as whole ones.

## Known limitations

- **Seeking past the transcode frontier** restarts the transcode from that point rather
  than jumping instantly. Within what has already been transcoded, seeking is immediate.
- **Chromecasts hardcode Google DNS** and ignore the DHCP-advertised resolver, so a
  hostname in `stream_host` cannot be resolved by the receiver and the load fails
  silently. Use a LAN IP, or a router-side redirect of the device's port-53 traffic.
- **Subtitles are dropped on the browser HEVC path**, because burning them in would mean
  re-encoding the thing that path exists to avoid.
- **Old Chromecast firmware (CrKey 1.56)** constrains the HLS output: v3/v4 manifests
  only, MPEG-TS segments rather than fMP4, and a synthetic master playlist carrying
  explicit codec, bandwidth and resolution hints.
- **The intro clip** forces a full re-encode of both the clip and the stream, adding
  about thirty seconds to startup.

## API

The CLI and the remote are both clients of the same HTTP API. Grouped, not exhaustive:

| Group | Endpoints |
|---|---|
| Playback | `/search` `/play` `/stop` `/pause` `/resume` `/seek` `/seek-retranscode` `/volume` `/next` `/prev` `/status` `/progress` `/position` |
| Web remote | `/remote` `/home` `/recent` `/title-meta` `/poster/{size}/{file}` |
| VLC | `/vlc/{id}/ready` `/vlc/{id}/open.m3u` `/vlc/{id}/stream` `/vlc/{id}/sub.srt` `/vlc/control` `/vlc/pending` `/vlc/gone` `/vlc/enqueue-next` |
| Streaming | `/hls/master.m3u8` `/hls/playlist.m3u8` `/hls/{segment}` `/hls/init.mp4` `/hls_cache/{key}/{file}` `/stream/transcode` `/torrent/{id}/stream/{file_idx}` (loopback only) |
| Library | `/library` `/library/files` `/library/vlc.m3u` |
| Watch tracking | `/watched` `/watched-add` `/watched-remove` `/watched-rate` `/watched-backfill-ids` `/following` `/following/add` `/following/mark` `/following/remove` `/following/seen-seasons` `/continue/remove` `/pending-watched` `/pending-watched/resolve` |
| Queue and picks | `/watchlist` `/recommendations` `/show-notes` `/queue` |
| Chromecast | `/targets` `/cast-info` `/api/cast-config` `/api/position` `/api/position/reset` `/api/retry` `/api/seek-restart` `/cast-receiver.html` |
| Other | `/history` `/config` |

## Tests

```bash
cargo test
```

## License

MIT
