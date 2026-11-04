# Podcasts, audiobooks and the uplink governor (2026-11-04)

What spela does with audio that is not a film or a series, why it is built this way, what has been
verified, and what is still open. Code: `src/podcast.rs`, `src/audiobook.rs`, `src/uplink.rs`, the
handlers after `handle_vlc_library_playlist` in `src/server.rs`, the `podcast` and `audiobook`
commands in `src/main.rs`, and the Podcasts and Books modes in `static/remote.html`.

## The owner's words that set the scope

- *"it should be able to stream podcasts, make it happen"*
- *"If spela COULD find the audiobooks through torrents etc then we can leave them, do searches
  through spela's torrent engines/search-methods etc to verify."* (said while deciding which
  audiobooks on an old disk image had to be kept; the rule is: a book spela can find need not be
  stored)
- *"Did you not search more torrent sources? There are many in spela, right? Search all of them."*
- On seeding: torrents may seed to one to two times what they downloaded, *so we're not leeching*,
  but only if nothing more important needs the limited upload of a mobile (5G) connection.
- *"The audiobooks could be downloaded to the Audiobooks app on MacOS I guess, if feasible?"*
- *"spela could also automatically offer to remove books/pods after I've listened to them, if
  that's tracked in the mac Books app etc"*

## Why films-and-series search cannot find a book

spela's only torrent source is Torrentio. Torrentio scrapes some twenty sites on its own servers
(the Pirate Bay, 1337x, RARBG, TorrentGalaxy, Rutracker and others) and answers one kind of
question: streams for an IMDb or Kitsu id. Its manifest (`torrentio.strem.fun/manifest.json`) has a
single `stream` resource for `movie`, `series` and `anime`, id prefixes `tt` and `kitsu`, and no
catalog. A podcast or an audiobook has no such id, so spela's existing code cannot look one up,
whatever the sites themselves would allow.

## Podcasts

1. **Find the show** in Apple's podcast directory (`itunes.apple.com/search`, no key). The store
   country decides which national shows rank, so it is a setting: `podcast_country` in
   `config.toml` (default `us`), overridable per search with `country=`.
2. **Read its RSS feed.** The feed URL comes from Apple's answer for a show id, never from a client,
   which keeps the endpoint from being an open URL fetcher. `feed_url_allowed` adds a scheme and
   host fence (no `localhost`, private addresses or LAN names). Feeds are capped at 40 MB and cached
   for 15 minutes. Items without an audio enclosure are skipped; episodes sort newest first.
3. **Play the publisher's own file.** No torrent and no transcode: VLC, a Chromecast's Default
   Media Receiver and a browser `<audio>` element all play MP3/AAC from a URL.

Endpoints: `GET /podcast/search?q=` · `GET /podcast/episodes?id=&offset=&limit=` ·
`GET /podcast/vlc.m3u?id=&ep=&t=` (`t` = start seconds) · `POST /podcast/cast {id, ep, cast_name}`
(the audio URL is read from the feed server-side).

## Audiobooks

Searched by title in three places, merged by info hash (the highest seeder count any source
reports wins), and filtered to names that carry every significant word of the query:

| Source | What it gives | Why it is here |
|---|---|---|
| The Pirate Bay API (`apibay.org/q.php`, category 102) | live seeder counts | the largest general index with an audiobook category |
| Knaben (`api.knaben.org/v1`) | one index over the Pirate Bay, 1337x, Rutracker and others | reaches sites that refuse scripts |
| AudioBook Bay (`audiobookbay.lu`) | info hash and size per book page, **no seeder counts** | the largest audiobook-only tracker; it lists books the general sites do not |

Measured on 2026-11-04 from a home connection: 1337x, TorrentGalaxy and MagnetDL did not connect,
KickassTorrents and EXT answer scripts with a Cloudflare 403, Rutor answered but carried none of the
titles tried. Knaben covers the first group. A source that fails is reported by name in
`failed_sources` and in the remote ("Not reached this time: …"), never hidden.

Two things learned on the way: the Pirate Bay API refuses Python's default `urllib` client and
accepts curl or a browser user agent (the module identifies as a browser); and Knaben's request
must carry `search_field: "title"` with `query`, or it ignores the query and returns the most-seeded
torrents on the site.

Ranking: live swarms first by seeder count; then books no source reports a count for (AudioBook Bay
only), which are often alive; a reported zero last.

**Playing.** `POST /audiobook/start {hash, name}` builds a magnet with spela's public tracker list
and starts it through `start_torrent_for_play`, so the media cap and free-space floor apply exactly
as for a film (the floor refused the first test on a nearly full disk, as it should). The book's
torrent usually holds one file per chapter: `GET /audiobook/{id}/files` answers `ready: false`
until the metadata arrives, then the audio files in natural name order ("Part 2" before "Part 10").
`GET /audiobook/{id}/open.m3u` is a VLC playlist of every chapter; `GET /audiobook/{id}/file/{idx}`
Range-serves one chapter as it downloads; `POST /audiobook/{id}/cast {file, cast_name}` casts one.

**Which torrents may be streamed to the LAN.** `/torrent/*` stays loopback-only (security audit
H4). `/audiobook/{id}/file/{idx}` is open to the LAN, but only for torrents started as audiobooks
and only for their audio files. That list is kept by **info hash** in `audiobooks.json` in the state
directory, because torrent ids are per session: kept in memory, a restart broke every playlist link
and lost the seeding rule's knowledge of which torrents are books.

**Content type.** The shared stream handler labels every file `video/x-matroska`; a phone's
`<audio>` element refuses that, so the audiobook handler sets the type from the file extension.

## Chromecast controls for audio

An audio cast creates no `CurrentStream`, so `/pause`, `/resume` and `/seek` (which look for one)
cannot find it. `POST /audio/control {cmd: pause|resume|stop|seek, seconds}` acts on the device the
last podcast or audiobook was cast to. `cast_url` now loads `audio/*` content as `Buffered`, since a
podcast episode or a chapter is a whole file of known length, not a growing transcode.

## The web remote

Two more modes beside Movies and TV: **Podcasts** (shows → episodes → ▶) and **Books** (results with
seeders, size, file count and sources → ▶). The target decides how it plays: **VLC** gets the
playlist through the `vlc://` handler; a **Chromecast** is cast one file, with pause and stop in the
audio bar; **This device** plays in the audio bar's `<audio>` element and walks through a book's
chapters by itself. The audio bar deliberately survives navigation, like any music player, and
stops only on its own ✕ (the trailer rule in CLAUDE.md is about media that should not outlive its
page; this is the opposite case). Picking a target does not re-run a podcast or book search, since
there is nothing to re-rank. Under 560 px the four mode buttons show icons only, the chosen one
named. The quality control is hidden in these modes. Upstream failures answer **424**, not 502/503:
the remote reads those two as "spela itself is down" and would replace the reason with a generic
message.

## Command line

```
spela podcast search lex fridman
spela podcast episodes <show id>
spela podcast play <show id> [--episode N] [--cast NAME] [--start SECONDS] [--print]
spela audiobook search the body keeps the score
spela audiobook play <hash> --name "<title>" [--cast NAME] [--print]
spela audiobook save <hash> --name "<title>" [--no-import] [--dir PATH]
spela audiobook finished [--remove]
```

Without `--cast`, `play` opens VLC on the machine the command runs on (`open -a VLC` on macOS, `vlc`
elsewhere); `--print` only prints the playlist URL.

## The Books app on macOS (`src/books_app.rs`, runs on the listener's Mac)

**`save`** waits for the whole book to download on the server (it gives up only after five minutes
with no progress), fetches every chapter, and joins them into ONE `.m4b`: ffmpeg's concat demuxer,
a metadata file with a `[CHAPTER]` per input laid end to end, Apple's `aac_at` encoder (ffmpeg's
`aac` if absent) at 64 kbit/s mono or 96 kbit/s stereo, and the audiobook tag (`media_type=2`). A
book that already is one `.m4b`/`.m4a` is copied, not re-encoded. Chapter names drop what every
file name shares ("heart_of_darkness_1a_conrad_64kb" becomes "Part 1a"). The file is opened with
Books, which copies it into its own storage; once Books' library lists the title, spela deletes its
own copy, so the Mac's disk gets the space back. One file with chapters is the form Books handles
best; separate MP3s are reported to import as many titles or merge without chapter marks.

**`finished`** reads Books' library database (`~/Library/Containers/com.apple.iBooksX/Data/
Documents/BKLibrary/BKLibrary-*.sqlite`, table `ZBKLIBRARYASSET`: `ZCONTENTTYPE = 6` is an
audiobook, `ZISFINISHED` and `ZREADINGPROGRESS` say how far the listener is) for every book `save`
recorded in `~/.config/spela/books_saved.json`. A book counts as finished when Books marks it so or
progress reaches 99%. With `--remove`, `POST /audiobook/remove {hash}` stops the book's torrent and
deletes its files on the server (books only; any other torrent is refused). Books keeps its own
copy; removing it there is the listener's step, since Books offers no scripting for it.

## Seeding: how long, and how fast

**How long.** A finished torrent keeps seeding until its title is in the watch ledger, and a book
never is. So a book seeds until it has uploaded `book_seed_ratio` times its own size (default 2.0,
the owner's "one to two times"), then pauses; its files stay for the next listen.

**How fast: the uplink governor** (`src/uplink.rs`, on by default, off with `uplink_governor =
false`). The server cannot see the other devices on the LAN, but it can see what they do to the
line: when anything fills the upload, packets queue in the router and the round trip to the outside
grows. Every 5 seconds the governor times a TCP connect to two public anycast hosts; the lowest time
in the last ten minutes is the line's base delay, and the excess over it is queueing. Above 100 ms
the session's upload ceiling halves (down to 32 KiB/s); below 30 ms it grows by a tenth (up to the
fixed 100 Mbit/s maximum). This is the idea behind LEDBAT (RFC 6817), the congestion control
BitTorrent's uTP uses to step aside for other traffic, applied to librqbit's session-wide rate
limit. It applies to all of spela's uploads, not only books, which is also what keeps a download's
own acknowledgements moving on a narrow uplink.

Two library facts it rests on: librqbit's `upload_bps` is spent in **bytes** per second (the
existing `MAX_UPLOAD_BYTES_PER_SEC` comment says so), and the limiter waits for a whole 16 KiB chunk's
quota at once, so a ceiling below one chunk per second would stall uploads rather than slow them.
Hence the floor of two chunks.

Considered and not built: a fixed low upload cap (simple, but it either wastes an idle line or still
fills a busy one); pausing book torrents whenever the line is busy (pausing and resuming every few
seconds churns peers, and seeding itself raises the delay it reacts to); measuring the host's own
interface counters (on a server that is no longer the router, the other devices' traffic never
passes through it).

## What is verified, and what is not

Verified on 2026-11-04 against a separate test instance (own port, state and media directory):

- The unit tests in the three modules, and live tests (`cargo test podcast audiobook -- --ignored`)
  against the real directory, three real feeds and all three audiobook sources.
- Podcast search, episode list and VLC playlist through the HTTP API; VLC decoded the playlist with
  audio output disabled.
- A public-domain LibriVox book with seven files: started, its six audio chapters listed in order,
  a Range request answered 206, a non-audio file and an unknown book refused with 404, VLC decoded a
  chapter, and the book was still known after a server restart.
- `spela podcast search|episodes|play --print` and `spela audiobook search|play --print`.
- `spela audiobook save`: the LibriVox book became one 4 h 10 min `.m4b` (123 MB, AAC mono 64 kbit/s,
  six named chapters, audiobook tag) in 52 seconds; opened with Books, it was listed in Books'
  library as an audiobook with its duration, in Books' own storage. `spela audiobook finished` read
  its progress (0%); `POST /audiobook/remove` deleted the torrent's files and forgot the book.
- The web remote in a browser, desktop and phone widths: both modes search and render, a podcast
  episode and a book played in the audio bar (muted for the test), and a book moved to its next
  chapter by itself.

Not verified yet:

- A Chromecast: no cast device was on the network, so `/podcast/cast`, `/audiobook/{id}/cast` and
  `/audio/control` have only been compiled, not run against a receiver.
- The `vlc://` hand-off from the remote with a real VLC window (the playlists were checked with a
  headless VLC instead).
- The uplink governor against a real busy line (its control law has unit tests; its probing has run
  only in the test instance).
- Deployment: none. The server normally runs on another machine; nothing here has been built or
  restarted there.

## Next

- **Offer removal by itself.** `spela audiobook finished` is the offer when asked; running it on a
  timer and telling the listener (a notification, or the voice assistant) is not built.
- **Resume for podcasts.** VLC's playlist already takes `t=`; nothing records where a listener
  stopped.
- **A Chromecast plays one chapter at a time.** A cast queue of all chapters would need the
  receiver's queue API, which `rust_cast` does not expose yet.
