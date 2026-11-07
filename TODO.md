# Spela TODOs 🎬🍿

### ✅ RULED — 7 November 2026: VLC demotes Dolby Vision profile 5

**Ruled and built the same evening:** *"Just maybe demote profile 5 as it seems to display wonkily
in VLC"*; a trial of server conversion was judged not worth it. The name rule below is live
(`dolby_vision_without_fallback`, mechanics in `CLAUDE.md`, tier 1). Not built: the byte check and
the conversion. Delete this section once a few weeks of VLC plays have shown no green picture.

**Same evening, the ranking alone was not enough:** Local Bypass matches the episode on disk, not
the release, and the profile 5 copy played earlier was complete and bigger, so ▶ would have served
it again over the ranked hybrid (verified on the live server: `/vlc/1/ready` answered "on disk" with
the RAWR file).

**Built the same evening, on his yes ("Sure. Sounds great."): one judge for the list and the disk.**
The bypass had checked only part of the ranker's rules (episode, year, resolution, complete, not
corrupt, at least 75% of the chosen size), so a cam or dub copy on disk could also replace a ranked
original. Now `on_disk_copy_judged_worse` (server.rs) ranks the copy on disk like a listed release
(as itself when it is listed, so Torrentio's flags count; else by file and folder name) and refuses
it when it is worse than the chosen release on any part of the ranker's class (`RankBucket`: another
film, Dolby Vision, cam, dub, resolution, language); the 75% size floor stays. Both bypass paths use
it: the VLC one, and the Chromecast/browser play, which finds the chosen release by its magnet; a
play with no listed release behind it (a library tap) is not judged. Rejected: comparing the full
rank key, since size bands would refuse a copy only slightly smaller and download it again, which
the floor exists to prevent. Still owed: the `CLAUDE.md` § Local Bypass line, held back to keep that
file to one write per sitting.
The question as it was put:

VisionQuest S01E02 opened green in VLC. The pick was `...2160p DSNP WEB-DL DDP5 1 Atmos DV H
265-RAWR`, and ffprobe on the server read it as **DV profile 5, `dv_bl_signal_compatibility_id=0`**:
no HDR10 or SDR picture underneath, which VLC 3.0.24 cannot convert, so the colours come out
green and purple. Since 2026-09-05 Dolby Vision is only a last preference for VLC (`dv_preference`,
after resolution and size), lifted because nearly every 4K web release carries DV and because DV
had played fine; that evidence fits profile 8, which has an HDR10 picture underneath. So a 4K DV
release still beats every 1080p one. Options, recommended first:
- **Read the name, then check the bytes.** For VLC, rank a release that names DV and no HDR10 (the
  usual profile 5 shape; `DV HDR` hybrids are profile 8) below every release without DV, as the
  Chromecast gate does; and when the served file's ffprobe says profile 5 with compatibility id 0,
  move to the next source with a note. Cost: such a title drops to 1080p when no other 4K exists.
- **Name rule only.** Cheaper; a release that says only `DV` but is profile 8 is demoted wrongly.
- **Play profile 5 properly** in a player that converts it (mpv or IINA through libplacebo; VLC 4
  is still beta). Keeps 4K, but replaces VLC under the bridge, the handler and the remote's control.
- **Convert on the server before VLC gets it.** Profile 5's base picture is not standard, so this is a
  full re-encode (libplacebo reshaping with the Dolby metadata, then NVENC), never a remux. The
  server's ffmpeg 6.1.1 has `libplacebo` with `apply_dolbyvision` (checked 7 November); unknown is
  whether the GTX 1650 keeps up at 4K beside the embedder (1.2 GB of 4 GB VRAM free). A one-minute
  run on the VisionQuest file still in the media dir settles it.

### 🔓 OPEN — 4 November 2026: podcasts and audiobooks (built, tested on a test instance, not deployed)

His words: *"it should be able to stream podcasts, make it happen"*, and *"If spela COULD find the
audiobooks through torrents etc then we can leave them"*. Design, sources, what is verified and what
is not: [`docs/podcasts_and_audiobooks.md`](docs/podcasts_and_audiobooks.md). Owed, in order:
- [ ] Deploy to the server with `bin/deploy-server.sh` once it is reachable again, then one real
  play each: a podcast in VLC through the remote's `vlc://` hand-off, a book in VLC, both on a
  Chromecast (`/audio/control` pause and stop included).
- [ ] Watch the uplink governor's log lines under a real busy line (`uplink: upload ceiling …`).
- [x] Save a book to the Books app on macOS as one M4B with chapters: `spela audiobook save` (his ask:
  *"The audiobooks could be downloaded to the Audiobooks app on MacOS I guess, if feasible?"*). Built
  and run once with a public-domain book, 4 November.
- [x] List finished books and remove them from the server: `spela audiobook finished [--remove]`
  (his ask: *"spela could also automatically offer to remove books/pods after I've listened to
  them, if that's tracked in the mac Books app etc"*).
- [ ] Make that offer by itself: run `finished` on a timer and tell him (a notification or Ruby),
  rather than only when asked.
- [ ] Resume position for podcast episodes.

### 🔓 OPEN — 4 October 2026: the "good enough" bitrate levels are starting values

**Live.** Size stops counting in the ranker once a release carries enough bitrate for its
resolution (`enough_mbps` in `src/search.rs`; reasoning and the rejected alternatives in
`CLAUDE.md` § Hard-Won *Size stops counting once the picture is good enough*).

**To tune by watching, not by argument.** The levels are 30 Mbps H.264-equivalent at 2160p
(about 17.6 as HEVC) and 9 at 1080p, and a sufficient copy counts as lean up to twice the
smallest sufficient one. Two things would move the levels:
- A pick that looks compressed on the 4K monitor: the level is too low. Note the title,
  the resolution and the Mbps shown under the episode title.
- Auto leading with something far fatter than needed while a well-seeded smaller copy sits
  below it: that copy is under the level, so the level is too high for its class. The
  likely case is a Netflix or Amazon 4K series, whose own streams run about 13–16 Mbps
  HEVC and are therefore below the 2160p level today.

**11 October refinement:** Auto/1080p now give releases in the same quality/language group
and within a largest-anchored 10% size band the same size priority, so seeds can choose
between close alternatives. This addresses East of Eden S01E05 (8.43 GB / 52 seeds versus
8.17 GB / 17); the codec omission had made the bitrate estimate too decisive. The
large size differences and Dolby Vision preference discussed below remain separate.

**Seen on the live lists 4 October, working as ruled but worth his eye:**
- **A lean pick can have a very thin swarm.** Blade Runner 2049 on VLC now leads with a
  22.2 GB copy that has 6 seeds, the 211-seed remux next. Accepted on 4 October ("one is
  bound to notice"). If it turns out to stall too often in practice, the evidence to bring
  is how often the race or the walk had to rescue a lean pick.
- **The Dolby Vision preference now decides more than it was written to.** It sits after
  size and before seeds, and was meant to separate "two releases that are otherwise the
  same pick". With the ceiling, every sufficient release ties on size, so a 1-seed
  non-DV copy outranks a 6-seed DV one (Wonder Woman 1984). Seeds before the DV
  preference would restore the intent.

- **A thin swarm leads once its original-language tag is read (seen 2026-10-06).** Kleo
  S01E01 now leads with `...ITA.GER.1080p.NF.WEB-DL...MeM.GP.mkv` (3.28 GB, 1 seed) over
  `...GERMAN.1080p.WEBRip.x265-RARBG` (860 MB, 19 seeds). `GER` is now read as German, so
  the release is the original with a second track rather than a foreign one, and the
  fatter file wins on the size tier before seeds are looked at. The
  language reading is right; the pick is the size-above-seeds rule. Same question as the
  lean-copy item above: when the fattest copy has one seed, should seeds weigh sooner?
- **English subtitles on a foreign film count against a release.** `Amelie 2001 English
  subs BluRay Rip` lands in language tier 2 with the French-subtitled copies of other
  titles, since it names a foreign language and not the original. For a viewer who reads
  English subtitles it is the right release. A ruling could make "the original with
  English subtitles" a clean release.
- **A cam is read from the file name only.** The dub check also reads the torrent's own
  title and Torrentio's language line (`torrent_title`, `languages`). `is_cam_source`
  does not, so a pack whose title says `TS` and whose file name does not would pass. Not
  seen in a real list; check one before building.

**Not built, and not proposed yet:** AV1 is counted as H.264 (no efficiency weight), and
HDR is not a ranked axis, so an SDR 4K can lead an HDR one on thirteen seeds.

### 🔓 OPEN — 4 October 2026: the search and remote fixes, what is unproven and what is open

**Live:** the ranker is a sort key (it cannot crash the sort again), the 40-result cut
follows the final ranking, cam and telesync copies and releases of a different film are
marked and never play by themselves, a film that is not out yet is not searched, the phone
search field owns its own row, and the "unreachable" banner is decided by a liveness probe.
Mechanics in `CLAUDE.md` § Hard-Won Lessons.

**Not exercised, so still unproven:**
- **The banner on a real phone.** It is tested against a scripted fetch (eleven cases in
  `tests/spa_pure.mjs`), and Claude's built-in browser cannot run the remote's requests at
  all. The case worth one look: wake the phone on the search view after a few minutes, and
  the banner should either not appear or clear by itself within seconds.
- **The phone search bar on the device itself.** Measured in an emulated 375 px and 402 px
  viewport only.
- **A deliberate tap on a cam row, played through to the end.** The rest of "No release
  yet" was checked on 4 October against *Verity* (in cinemas since 30 September, six
  sources, all cam): the server refused the play on both `/play` and the job API with
  nothing started, and the remote's own code drew that result with the note and no ▶. What
  was not done is letting a tapped cam row actually download and play.

**Open idea, not proposed yet: a crashed handler should say so.** A panic in a request
handler closes the connection with no reply, which through the proxy is an HTTP 502. The
remote now reports that honestly ("Search failed"), but the server could answer with a
500 and the reason itself (`tower-http`'s catch-panic layer), which would also put the
cause in the remote instead of only in the journal.

### 🔓 OPEN — 3 October 2026: the play path is bounded and live; what is still open

**Live on the server (commit `aec8d7e`).** A play is a server job the remote polls
(`/play/start`, `/play/jobs/{id}`), so it no longer depends on the phone keeping a
request open through the warm-up. One Play or Stop owns the transcoder, and no wait in that
hand-over is unbounded. Every Chromecast conversation has timeouts. The automatic fallback
walks the whole ranked list inside five minutes and announces each switch. Local Bypass
finds an episode inside `release/Season 02/`. Failed and discarded torrents are paused,
not deleted. The mechanics and their constants are in `CLAUDE.md` § Hard-Won Lessons
(*Play ownership and bounded waits*, *Source walk*, *Chromecast connection*).

**What was run against real hardware on 3 October** (an isolated test instance, then
production): a film whose first two sources were dead walked to the third and reported
both switches on `/progress`, loaded on a Chromecast in 7 s and played; Stop answered in
0.3 s and the TV went idle; Stop pressed 6 s into a warm-up answered in 8 s and the job
ended as cancelled; production reads Chromecast status through the new connection code.
The loading panel's handling of the note is covered by `tests/spa_pure.mjs`.

**Not exercised, so still unproven:** a Chromecast that stops answering in the middle of a
LOAD (the 60 s ceiling and the STOP queued behind it); a real phone suspending its browser
during warm-up (the job API was driven by a script); and the loading panel watched in a
browser during a real walk. The first needs a device that misbehaves on demand, the other
two need one play from a phone.

**Decided 3 October: best picture stays the lean, and a copy on disk is always shown.**
- A zero-seed result is tried in its ranked position and that stands. It follows the
  2026-09-06 rule that seeds prefer and never exclude, and one of the two "zero seed"
  sources in the 3 October run delivered and played.
- The walk does not jump to a smaller copy on disk after a failed pick. Instead the title
  card now states a complete copy on disk (`onDiskOffer` in `remote.html`): a plain note
  when it is the source ▶ would play, a button naming its resolution and size when it is a
  different release, and an action toast on the fast-play path, which never shows the
  card. Until now the copy was only visible inside the collapsed source list.

**Open, the viewer's call: a much smaller copy on disk still wins silently on the
Chromecast path.** The VLC path refuses a copy below 75 % of the picked release's size
(`bypass_size_is_acceptable`, 2026-09-05) and fetches the pick. The Chromecast path
(`do_play`) has no such floor, so a complete top-level file of any size plays in place of
the pick. The test `a_much_smaller_copy_on_disk_is_judged_differently_by_the_two_play_paths`
pins both halves. Giving the Chromecast path the same floor makes "best picture" hold on
the TV as well; the cost is that a half-watched episode whose best source has since
changed would resume from a new download rather than from the copy on disk, unless the
on-disk button is tapped.

**Diagnosed, not built: a season pack's files are looked up at the media root.**
Persistence and file lookup assume a torrent's files sit directly under the media root,
while librqbit puts a pack inside its own release folder. The session that found this
on 2 October read `0 physical bytes` in the log for a pack whose episode was on disk; that
log line has not been re-read since. None of the correction is in the code.

**Left on the server.** A hard link at the media root for one episode that sat inside a
season folder, made on 2 October so the shallow scanner could see it. It is redundant now
that the nested lookup is live, costs no space, and the 168 h top-level sweep removes it.

**Lint is not clean, and nothing enforces it.** `cargo clippy -- -D warnings` reports
about 20 style lints in non-test code and 40 with tests (doc list indentation, complex
types, one manual prefix strip). The repository has no CI.

### 📌 NOTE — a failed torrent init leaves a 0-byte file named like the release (2026-09-16)
Twice in four days librqbit logged `Error setting length for file "<release>.mkv" to
<N>: file is None` during `initialize_and_start` (Contact's remux 2026-09-13, Terminator 3's
remux 2026-09-14 — both large REMUXes, which may or may not be coincidence). Each left a
**0-byte file** at the release's exact name in `/mnt/hdd/spela-media`.

Benign today: `top_level_file_is_healthy` requires ≥100 MB non-sparse, so Local Bypass
cannot pick one, and the 168h top-level file sweep eventually removes it. Recorded because
it is a **failed start that leaves litter behind**, which is the same subsystem as the open
1.2 GB-vanishing item above, and because a 0-byte file sharing a release's exact name is
the kind of thing a future size- or name-based check could trip over. Not investigated.

### 🔓 OPEN — the ranker has no idea what the VIEWER'S LINK can carry (2026-09-11)
Ranking is right film → addressability → Dolby Vision → cam → dub → resolution → language → codec → bitrate → seeds (README §
*How sources are ranked*). Nothing expresses **bytes per
second available to the person watching**, so the same #1 pick is served to a Chromecast
on gigabit LAN and to VLC on a phone hotspot two hundred km away over WireGuard. On
2026-09-11 that meant a 36.8 Mbps remux over a hotspot: unwatchable, and nothing in the
ranking could have known.

The bitrate is derived at rank time since 2026-10-04: the runtime travels as
`RankOpts.runtime_min` and `is_enough` turns a size into Mbps. That work also softened the
worst case, because Auto no longer leads with a remux when a smaller copy is good enough.
What is missing is the OTHER half: what the requester can actually sustain. A declared
limit would now be a small change, a lower ceiling for that request.
Options, cheapest first:
- **Declared**, per target: a `max_bitrate_kbps` on the play/search request, set once in
  the SPA's target picker (LAN / VPN / hotspot). Deterministic, no measurement, and the
  viewer knows their own link better than a probe would in the first 10 s.
- **Observed**: derive it from the last stream's delivered throughput to that client.
  Accurate, but arrives too late for the first pick of an evening.
- **Inferred**: the requester's address (LAN subnet vs a WireGuard peer) as a coarse prior.
  Free, but a hotspot and a fibre line look identical from behind the tunnel.

Leaning declared-with-an-observed-correction. **Decide the UX first** (does the target
picker grow a link setting, or does it live somewhere else?) — that is Fredrik's call, not
an implementation detail. Until then the Local-Bypass tie-break shipped 2026-09-11 stops
the worst case (being handed a remux when a small copy of the same film is on disk), but
does nothing when the only copies are large.

### 🔓 OPEN — a 1.2 GB partial download vanished across a restart (2026-09-09)
Star City S01E03 reached 1.2 GB and `open_pct 100`; after a `systemctl restart` it
was absent from the persistence store, its directory was gone, and a re-tap started
from 27 MB. **The pruner is exonerated by its own silence** (no eviction line) and
the boot reconciler by its own log (it recorded forgetting only a different,
genuinely empty torrent). The two paths that were never ruled out,
`race_torrent_sources` losing-candidate cleanup and `stop_torrent(.., delete_files =
true)` on a failed start, now pause instead of delete (live since 2026-10-03; no caller
passes `true` any longer). The reconciler was made incapable of deleting files on the
first evening. None of the three was proven guilty: removing the capability is cheaper
than defending it. **What deletes media files now:** the pruner (`disk.rs`, which logs
each eviction) and `do_cleanup`'s scratch directories. **Next action**: none unless it
recurs; a recurrence with these paths closed would point at librqbit itself.

### 📌 NOTE — the subtitle warm logs once per readiness poll while waiting
`no English subtitle found yet for <imdb>_eng` appears every ~1.5 s for the whole
buffering window (observed 2026-09-14: a solid two-minute run in the journal). It
is BOUNDED — it stops when the wait does, and `sub_missing` already negative-caches
the fetch itself — so this is log noise, not a leak or repeated work. Worth one
line of suppression the next time `handle_vlc_ready` is open; not worth a trip of
its own.

### 🔓 OPEN — unpin the librqbit fork when the upstream PR merges
`Cargo.toml` points at `semikolon/rqbit` by commit for one fix: the incoming-listener
starvation (ikatson/rqbit#663). **Next action**: watch that PR; when it lands in a
release, move back to crates.io and delete the fork note in `OPERATIONS.md`.

### 📌 NOTE — one laptop sync printed no trigger lines; not reproducing
The 2026-09-05 09:24 sync deployed 27 templates and reported `ok` with no
`nit: trigger '…' succeeded` lines at all. The 11:2x sync printed three,
including the new `42-restart-spela-vlc-bridge`. Unexplained; nit's own logic
counts a new trigger as changed, so it should have fired the first time. Watch
for a recurrence rather than chasing it — a bridge change that appears not to
take effect is the symptom, and the manual fallback is
`bash scripts/darwin/42-restart-spela-vlc-bridge.sh` (in the owner's dotfiles).

### 🔓 OPEN — see a next-episode hand-off end to end

Next-episode enqueue into the running VLC is shipped and the serving half is
verified; what has never been watched is an actual hand-off between two episodes,
because it is LOCAL-ONLY and the obvious test episode was a sparse placeholder.
**Next action**: download E02 first, then play E01 to the watched mark and watch it
roll over. Detail: CLAUDE.md Hard-Won "Next-episode ENQUEUE".

### Open — recommender + ledger follow-ups (2026-08-27)
- **Rating — SHIPPED 2026-08-27.** `WatchedEntry.rating: Option<i8>` (`1` loved / `0` fine / `-1` no; `None` = UNRATED and MUST stay distinguishable from "fine"). `POST /watched-rate`; rating a SHOW applies to all its episode rows and a re-mark carries the judgement across. Rewatch renders TWO toggles, never a cycle, plus a Loved-only filter.
- **imdb_id backfill — DONE 2026-08-27.** `POST /watched-backfill-ids` resolves ONE id per distinct title via `title_meta` with `auto_kind`, applies it to every row, idempotent — re-run it whenever the ledger grows. Ran clean: 107/107 rows carry a well-formed id.
- **`media_type=auto` is opt-in**; the To-Watch and rec rows still pass an explicit hint because they know it. Do not flip the default — OMITTING the hint has always meant "movie" and existing callers rely on it.

### Netflix viewing-activity import — OPEN, blocked on Fredrik (2026-08-26)
The structural cure for the lifetime-history gap (see § Watch tracker for why it matters — it produced five already-seen recommendations in one session). **Blocked**: the Bitwarden vault is logged OUT (`status: unauthenticated`, `lastSync: null`), so the Netflix credentials cannot be looked up. **Fredrik runs `bw login` then `bwu`** (master password + 2FA, human-only); then Claude searches the vault, pulls the PER-PROFILE CSV from `netflix.com/viewingactivity` (never the account-level GDPR export — it pulls every family profile), and builds the importer.

### VLC readiness follow-ups

- Evaluate sustained slow-swarm behavior using measured delivery and contiguous runway.
  The old 15% gate is removed; do not implement it again. The full-duration runway
  formula is an unimplemented alternative, not the live fast-start policy. Current
  thresholds and preserved rationale: [playback readiness](docs/PLAYBACK_READINESS.md).
- Investigate DV profile-5 rendering on VLC before changing target-specific ranking;
  missing codec metadata alone must not penalize an otherwise comparable source.
- Audit detached `prefetch_ends` / `prefetch_at` cancellation and repeated invocation
  under stalled swarms; `startup_buffer` itself is bounded and cancels reads on return.

### Release-availability robustness

- Extend automatic availability reconciliation to the TMDB fallback when no verified
  TVmaze schedule is available, without accepting mismatched show identities.
- Preserve pending/positive availability across restarts if older missing episodes
  need reliable Home rechecks; the current in-memory cache loses that evidence.
- Model batch-release holes separately from latest-episode high-water marks before
  promising per-episode availability for every counted new episode.
  Constraints and current cadence: [episode availability](docs/EPISODE_AVAILABILITY.md).

### VLC audio/seek reliability
- **Away-from-home playback — `spela-vlc` shipped on the laptop 2026-08-13; fleet-ify it.** One-command watch on a VPN'd Mac: `~/.local/bin/spela-vlc "<title>" [--movie]` searches spela + opens the top source in VLC (`…/open.m3u`, subs+resume) → HDMI/Cast to a foreign TV (spela can't cast to a non-home-LAN TV). Currently the laptop-only + UNTRACKED → weave into dotfiles/`~/.local/bin` so every Mac gets it + it survives reprovision. Optional upgrade: install the `vlc://` handler on the laptop (fussy on Big Sur over SSH) so the web remote's "Open in VLC" button works one-tap there. Full: CLAUDE.md Hard-Won "Watching AWAY from home".
- **Metered-connection detection — DONE 2026-09-06.** Off-LAN (`10.x`, i.e. WireGuard, which is the only way in from mobile data or a hotspot) defaults the search to Saver when no preference is asked. `X-Forwarded-For` is read but trusted ONLY from a local peer, since the remote arrives through kamal-proxy. The known imprecision stands and is accepted: off-LAN is a superset of metered, so a laptop on a café's wifi is capped needlessly — the quality control covers it. `navigator.connection` was rejected: `effectiveType` measures speed, not cost.

- **Bitrate in the quality label — DONE 2026-09-06.** Shipped as an ffprobe of the file actually being served, not from VLC's `stats.demuxbitrate` (instantaneous, moves with buffering, undocumented unit) and not from the release name (a claim; two proofs in this project that it lies). File bitrate = size ÷ duration, total rather than video-only. Verified against an independent ffprobe on the same file: 6,173,439 bps reported vs 6,173,440 measured. Probed once per path in the background, cached, keyed by path so a Bypass substitution changes the label with the bytes.

- **Auto-subtitles on VLC — the live-verify RAN 2026-09-05 and FAILED; fixed, verified end to end.** VLC ignores `input-slave` from a playlist file, so the whole OpenSubtitles fallback had reached VLC exactly never (measured: same URL fetched as a CLI flag, zero requests from an m3u). The `vlc://` handler now passes `--input-slave`; subtitles are cached per episode key and warmed from `/vlc/{id}/ready` (cold 39.7 s → warm 0.007 s), and a missing one is negative-cached because VLC retries a failing slave ~4 times. The ranker also gained a language tier — a Polish AI-dub and a French MULTI had been outranking the plain English release on seeds alone. Full: CLAUDE.md Hard-Won "A subtitle reaches VLC only as a COMMAND-LINE flag". **Still open:** library→VLC subs (`/library/vlc.m3u` has no sub endpoint, so `--input-slave` is only attached for `open.m3u` plays).
- **Library→VLC seek — FIXED 2026-08-05 (serve-library sliding TTL).** VLC seeks died ~2 min into a library watch (handle expired at the 120 s TTL → 410 GONE → "input can't be opened"). Now the handle TTL is SLIDING (refreshed on every stream access) + a 6 h base. Full: CLAUDE.md Hard-Won "serve-library handle TTL is SLIDING". **Follow-up (open): library→VLC "never a dub"** — `/library/vlc.m3u` serves the raw file, so VLC picks the container default audio track; pass the TMDB `original_language` as an `?al=`/`#EXTVLCOPT:audio-language` hint (the vlc:// handler already forwards `?al=`). Needs the library entry to carry `original_language`.
- **Audio drops — device-pin CURE SHIPPED 2026-08-04 ✅ (one live-verify left).** The macOS auhal output module was resetting mid-stream when VLC followed this Mac's system-default-device churn (8 devices; VideoLAN #8556). Cure: the `vlc://` handler now resolves the Argon DA2 DAC's current CoreAudio id FRESH each launch (by its stable Burr-Brown UID via `SwitchAudioSource`) and passes `--auhal-audio-device=<id>` — self-correcting against reboot/replug id drift; DAC absent → no flag → graceful fallback. First attempt (`--audio-device=<UID>`) was an INVALID VLC option and broke launch (Built≠Verified); the correct flag `--auhal-audio-device=<numeric id>` is verified-accepted. Fredrik confirms drops stopped. `switchaudio-osx` tracked in the Mac's Brewfile. **Remaining:** confirm the pin holds across a real reboot/replug that shifts the id (resolver handles it by construction, unproven on an actual drift event). Full: CLAUDE.md Hard-Won "VLC audio drops + seek-kills". Monitor: `~/.local/bin/spela-audio-drop-monitor`.
- **Seek-kill recovery can land on a partial source.** 2026-08-03: a backward seek dropped a HotD S03E06 stream (complete 588MB MeGusta), and the re-open played result #4 — a *different*, still-downloading 3.8GB release — via FileStream, so it couldn't resume. Consider: for the VLC path, prefer a COMPLETE non-sparse on-disk copy of the episode over a sparse/partial one regardless of which result id was tapped.
- **Amp signal-sense keepalive during playback (parked).** Earlier amp-mute hypothesis was DISPROVEN (VLC decoded audio fine through the silence); the during-playback keepalive idea is moot for this bug. Only revisit if a genuine idle-APD case appears.

### VLC remote control (v3.22 — shipped, one live-verify left)
- **Confirm the full couch-loop live.** Web remote → VLC control is deployed (relay: `POST /vlc/control` → watcher drains `/vlc/pending` @1s → VLC HTTP cmd). Endpoints round-trip-verified + the watcher reloaded, but the end-to-end (tap play/pause/seek on the phone → VLC-on-the-Mac responds within ~1-2s; scrubber tracks VLC's playhead) wants ONE real watch to confirm. If a command lags >2s, the watcher's 1s drain tick or the `spela-vlc-bridge` agent (check `~/.local/share/spela-vlc-bridge.log`) is the place to look.
- **VLC now-view liveness/ending.** It does NOT auto-clear on `!vlc_active` (that flag is only fresh ~30s after VLC's last byte fetch — a buffered-ahead quiet VLC would look "ended"). The user stops via the Stop button. If a cleaner auto-end is wanted, gate on "position not advancing AND /status quiet > ~3min".

### Graceful restart (shipped + deployed 2026-08-04, one live-verify left)
- **Confirm Chromecast boot-resume across a restart.** Socket-activation is proven live (both listeners inherited, no address-in-use) and VLC reconnect-across-restart is verified (VLC re-requested a mid-file byte-range and resumed after a real restart). The untested path is `maybe_resume_stream_on_boot`: after `systemctl restart spela` with a live cast, the Chromecast should re-establish (transcode + cast from the persisted `CurrentStream`, recency-guarded ≤180min) at the right position. Needs Fredrik present (a restart interrupts the cast). Kill-switch `SPELA_NO_RESUME=1` if it misbehaves.

### v3.19 open loops (2026-07-29 — followed-shows tracker + VLC playback tracking)
- ~~VLC-torrent FD leak~~ **FIXED 2026-08-04** (`reap_previous_vlc_torrents`): each new Open-in-VLC play stops the previously-started VLC torrent (you watch one at a time), keeping the data on disk for Local-Bypass. Bounds VLC torrents to the current stream, so per-play torrents can't accumulate into `EMFILE` again. `LimitNOFILE=524288` (drop-in, hemma overlay) stays as the ceiling. Residual (harmless): the LAST torrent lingers after VLC quits until the next play or a restart — 1 torrent's FDs is negligible.
- **Follow-list curation — now UI-driven (v3.20).** Follow/Unfollow buttons on the search card + an unfollow ✕ on New-Episodes rows; no manual JSON editing. `following.json` (the server, user-local — never the public repo) is a membership set; progress lives in the ledger, so unfollow/re-follow is lossless. Residual: none beyond adding shows as viewing changes.
- **VLC watcher end-to-end validation.** Confirm on a real VLC watch that `/position` syncs the resume HWM live + completion auto-marks (heartbeat in `~/.local/share/spela-vlc-bridge.log`).
- **VLC web-interface persistence.** Verify `vlcrc extraintf=http` + `http-password` survive normal VLC quits (VLC rewrites `vlcrc` on quit — it may need re-setting or a guard).
- **spela-vlc-bridge plist not nit-tracked.** `~/Library/LaunchAgents/*.plist` is gitignored; the watcher *script* IS nit-tracked but the plist isn't → add a deploy trigger so a fresh Mac provision installs the agent (fresh-provision gap).
- **Opus 5 / CC auto-update — self-heals, NO action.** The z.ai `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` var (which had frozen the native updater at 2.1.172) was stripped; the updater resumes on fresh `claude` launches, so ≥2.1.219 + Opus 5 lands on its own. Just confirm eventually.

### v3.14 + peer-supply — SHIPPED (2026-07-05 overnight barrage) + OPEN LOOPS
A 20-title barrage (blockbuster → Vampyr 1932) on an isolated ephemeral-DHT test instance (7891) drove these to production:
- **pkill cross-instance-kill fix** — `do_cleanup` scoped to `<media_dir>/transcoded_hls` (was SIGKILLing production's live ffmpeg from a *test* instance's cleanup; the ORIGINAL looping-Silo cause).
- **alass densest-track + fast-extract** — wrong-release SRT 6-min-offset fix; picks the densest embedded text track (non-forced-first, stop at first ≥20-cue) = ~1 extraction (was extracting all 6 = 16s "Preparing").
- **torrent pre-buffer 20→10 segments** — ~45% faster start (barrage median 44s→24s, 0 buffering; validated by a 3-min sustained play racing 4→124 segments ≈ 4× realtime).
- **20-tracker injection** (session-wide `SessionOptions.trackers`) — peer-supply for thin swarms (Vampyr 62→23s, Hundstage 0-seed >75→22s).
Data + research: `docs/barrage_findings_2026_07_05.md`, `docs/slow_torrent_strategies_research_2026_07_05.md`, `docs/librqbit_streaming_faststart_research_2026_07_05.md`.

**OPEN LOOPS (Fredrik decisions + deferred work):**
- **[TRACKER-RESILIENCE THREAD CLOSED 2026-08-05 — Fredrik accepts a few outages/year; no fallback built]** Explored mirror-fallback + second-scraper resilience after a Torrentio 522 outage. Findings: **no config-less public second-scraper is reachable from a server** — Comet (elfhosted) is Cloudflare-blocked (403 even with a browser UA); KnightCrawler 302s to /configure; MediaFusion returns 0 config-less; Jackettio returns a *"kindly configure this addon"* nag (0 real streams); and Torrentio has effectively one host, so "mirrors" aren't a thing. The genuinely-robust options were TorBox debrid ($3/mo, also the 4K cure — below) or self-hosting KnightCrawler on the server (Postgres + indexer stack + maintenance). **Fredrik declined TorBox and the self-host** — the shipped 522-retry (8 s timeout, 2 attempts, clear "temporarily unavailable, try again" message; `search.rs::torrentio_streams`) covers the common blip case, and a full Torrentio outage (~1-3×/year) is tolerable. Do NOT re-propose unless Fredrik reopens it (revisit only at launch). Detail on the shipped 522-resilience: CLAUDE.md Hard-Won "Torrentio 522/non-JSON".
- **[PARKED — Fredrik decision, revisit at launch] TorBox debrid fallback — now also the 4K-reliability cure** — the *other* cure for obscure/slow swarms (~$3/mo, ~200 lines `reqwest`). Deliberately deferred 2026-07-06 (Fredrik: no $3/mo yet; will consider "in future or when I launch/market spela more widely"). **Sharpened 2026-07-13**: this is *the* answer for 4K. HotD S03E04's 4K sources would not download at all — the DV releases connected 1 peer / 0 B/s, the "344-seed" non-DV connected 3 real peers / 0 B/s (Torrentio seed counts are inflated tracker-scrape claims, not deliverable capacity). The 4K browser pass-through is built + ready, so debrid is the *only* missing piece for reliable 4K playback (it holds the file → "seeds" stops mattering). Would slot in as an extra racer alongside the local sources (try local → fall to TorBox if no byte-0 in N s → cached = instant HTTPS). Config-gated (absent token = today's behavior, zero regression). Full API flow + fallback-ladder: `docs/slow_torrent_strategies_research_2026_07_05.md` § 6. Secret in a tier; SSRF-validate the `*.torbox.app` CDN host at the boundary.
- **[DONE + VERIFIED end-to-end 2026-07-13] Browser 4K/HEVC pass-through** — a HEVC source to a browser target is stream-copied into fMP4/CMAF (hvc1, audio→AAC), full 4K, no re-encode/downscale. **VERIFIED**: Chrome/hls.js decoded a real 2160p HEVC at 3840×1920, playing. Reaching "works" needed a fix-chain (all in CLAUDE.md Hard-Won): ffprobe field-order parse (`video_codec` came out empty → gate silently failed; `codec_name` precedes `codec_type`), **HEVC→browser is ALWAYS fmp4, not just 4K** (regular transcode copies HEVC into mpegts which browsers can't decode → 0 segments → fail-fast), master CODECS `.B01`-tail strip (Chrome MSE rejects `hvc1.2.4.L150.B01`), hls.js buffer bounded for huge 4K segments (else SourceBuffer quota-overflow → spin + reseek jumps), bare-live playlist so ffmpeg's ENDLIST→VOD kills the live-sync frame-jump. Runway 12s/10s→30s (2160p), search cap 8→40. Commits `abba80c`…`4b397c0`. **Follow-ups**: (a) auto-select best 4K on a 4K display (today it's a manual "More sources" pick); (b) prefer non-DV over DV for browser 4K (profile-5 tint; 8.1 fine) — the VLC target sidesteps this (VLC decodes DV cleanly).
- **[SHIPPED 2026-07-13] Open-in-VLC** — "VLC · this Mac" target → opens the source in VLC (native HEVC/DV, full seek). `/vlc/{id}/stream` (Range; all 3 states: complete-file / partial / fresh-torrent FileStream) + `/vlc/{id}/open.m3u` (carries `#EXTVLCOPT:audio-language`+`?al=` = original-language audio rule). One-click `vlc://` handler tracked at `scripts/install-vlc-url-handler.sh` in the owner's dotfiles; SPA fires vlc:// + .m3u fallback. **Follow-up (deferred, needs a restart):** picking VLC from the *now*-view doesn't open the current title (only a fresh source tap does) — store `S.curRid` + route the now-view VLC target through `openInVlc`.
- **[SHIPPED slices 1-2-3arsenal, 2026-07-13] Watch tracker (spela replaces TV Time — free/no-ads/no-sub).** Slice 1 `/recent` "Recently watched" wrapping row (last-14d distinct plays; tap→jump-to-show; NOT resume). Slice 2 watched-ledger (`AppState.watched` auto-marked at completion, `/watched`). Slice 3 ARSENAL: `/watchlist` + "To Watch" nav view (RT-scored rows, <88 dimmed, watched hidden, tap→search) serving `~/.config/spela/watchlist.json` (seeded from RT import, 54 movies + 17 unwatched series). **⚠ arsenal data lives on the SPELA HOST (the server `~/.config/spela/`), NOT the Mac — editing on the Mac won't reach spela; scp to the server.** **NEXT (trued 2026-08-04):** slice 2b Up-Next next-unwatched episode is SHIPPED (`next_unwatched_skipping` → `/home` surfaces `next_unwatched` + air date); slice 3 HARNESS has RUN (`recommendations.json` live on the server — Claude-curated picks with `why` rationales + `taste_profile.md`). Genuinely remaining: the airdate→**push-notification** when a followed show's new episode drops; slice 5 auto-track house Chromecasts (cast-status polling; bg-loop needs Fredrik's OK per bg-task-guard). Roadmap: `docs/watch_tracker_roadmap.md`. Personal data USER-LOCAL in `~/.config/spela/`. **Open capture:** TV Time GDPR export is empty of history (verified); Fredrik screenshotted the completed-series profile pages but iCloud sync is full — pending; naming loved shows into `watchlist.json`/taste profile meanwhile. **Structural fix for the whole gap: a one-time watched-history import into the ledger** — the recommender still guesses what he's already seen because the ledger is recent-spela-only, not lifetime. **Sharpened 2026-08-26** after the gap bit FIVE times in one session (The Lowdown, The Pitt, Oxygen, I Am Mother and Upgrade were all recommended and all already loved): the recurring variable is the LEDGER, not the picks, so a recommender should SAY it is guessing rather than present picks as discoveries (now written into `taste_profile.md`). **Newly-identified path — Fredrik had assumed none existed**: Netflix publishes full per-profile viewing activity at `netflix.com/viewingactivity` as a downloadable CSV, and Amazon has an equivalent data request; between them that is likely a large slice of a lifetime of viewing (Apple TV has no export). **Use the PER-PROFILE page, not the account-level GDPR export** — he has always used his own profile on the family account, and the account export pulls everyone. **Blocked on**: the Bitwarden vault is logged OUT (`status: unauthenticated`, `lastSync: null`), so the Netflix credentials can't be looked up — Fredrik runs `bw login` then `bwu` (master password, human-only), then Claude can search the vault and build the importer. Trakt/Letterboxd remain the fallback if the Netflix route disappoints.
- **[DONE 2026-07-08] Persistent play-error/retry surface + mobile layout** — a failed play now shows "Couldn't start — <reason>" + Tap-to-retry ON the now-view (was toast-only + bounce-to-Search). Mobile: icon-only nav + Chromecast pill, full-width search result, sideways-overflow killed (`@media(max-width:560px)`). Commit `9341379` + the layout commit. UX-doc §6/§9 + §7 cheat-sheet now fully shipped Claude-side; only the custom-receiver track (subtitle-as-track, intro-warmup) remains, gated on Fredrik's $5 Cast registration.
- **[DONE 2026-07-06] WAN inbound listener re-opened** — TCP 6881 accept re-added to the WAN iface (`enx00e04c680072`) in iptables INPUT, persisted in `dotfiles/system/<server>/etc/iptables/rules.v4`. The 2026-07-05 precautionary close was reverted: the e1000e hang's root cause was the NIC TSO/GSO offload bug (fixed by offloads-off + `eno1-nooffload.service`), NOT the listener — the hang was on `eno1` (LAN) while the listener is on the WAN iface. CrowdSec per-IP bans still cover 6881. (µTP inbound over UDP 6881 not opened — only the TCP peer listener; add UDP if µTP inbound is wanted later.)
- **[DONE 2026-11-04, his word: "Ok go ahead"] Public-repo personal data** — the tracked files no longer carry this deployment's machine names, addresses, TV names, place or household: code and docs say "the server", "the renderer", "the library disk", `media-server.lan`, `192.168.1.x`. Four values that were built into the code now come only from `config.toml`: the allowed hostname (`allowed_hosts`), the drive-alert topic (`library_ntfy_url`, default none), and the renderer target (`renderer_watch_url`, `renderer_name`; the target is offered only when set, and `target=shannon` is still accepted). The owner's deployment notes and the table of what each generic word means there are in `CLAUDE.local.md` (gitignored). History keeps the old text; the owner chose not to rewrite it. Why it mattered beyond privacy: a friend can now install spela without inheriting this deployment's hosts.
- **[DONE 2026-07-06 — local variant, LIVE on prod] Source racing (no debrid)** — weak-swarm-gated pre-selection stage in `do_play`'s torrent branch: when the top pick has `seeds < race_seed_threshold` (default 100), the top `race_max_sources` (default 2) releases are added to the shared librqbit session concurrently, the first to deliver real bytes wins, losers are stopped+deleted, winner continues via `AlreadyManaged`. Config-gated (`race_sources_enabled`, default on). **Validated on the test instance** (The Cremator 4-seed → result 2 delivered first, won in ~1.3s, downloaded at 843 KB/s). Two commits: `2857992` (feature) + `047bd75` (concurrent-start fix — `add_torrent` blocks on magnet metadata, so a dead candidate-1 hung the sequential version). Debrid leg parked (above). GPU use unchanged (race is download-only; only the winner transcodes).
- **[good hygiene] deliberus-postgres binds `0.0.0.0:5432`** — WAN-blocked (INPUT drop + DOCKER-USER) so LAN-only, but should bind the LAN IP like the other DBs.

### NEXT — intro-warmup: instant TV wake + parallel handshake (Fredrik's idea, 2026-07-04)
The ~8s Chromecast LOAD handshake (connect + launch receiver app + buffer) runs SERIALLY *after* race-ahead. Fredrik: cast the (currently-disabled) intro clip as a warm-up placeholder the instant the transcode starts → TV lights up in ~2-3s (huge perceived-perf win) + the receiver-app-launch overlaps the transcode buildup → then LOAD the real HLS on the warm receiver. Build defensively (best-effort; any failure falls through to the normal cast, no regression). Timing caveat: intro is 5s, real-ready is ~13s → a gap where the intro ends before real content; consider looping the intro or a longer placeholder. NEEDS live-Chromecast testing (the swap-glitch) — can't fully verify without a device.

### NEXT (the REAL fix for BOTH speed + subtitles) — custom Cast receiver ($5 registration)
Fredrik's insight (correct): Cast receivers render subtitles as a **layered text track**, NOT burned in. With a custom receiver we can (a) cast the real HLS as soon as 1 segment exists (~2-3s) and let the receiver buffer gracefully — no 12s pre-buffer wait; (b) attach the subtitle track SEPARATELY, aligned in the BACKGROUND, added after playback starts — eliminating the subtitle-blocks-start cost (the 19s cold-cache alass) entirely. **Fredrik plans to do the $5 Cast SDK Developer Console registration ~2026-07-05 — unblocking imminent.** Once registered, this becomes the top spela priority: it subsumes BOTH the intro-warmup (cast-early, receiver buffers gracefully → no ~8s handshake wait, no cast-connection-reuse refactor needed) AND the alass-on-critical-path problem (subtitles as a background-aligned track added after playback starts). Design + build it WITH Fredrik (needs live-receiver iteration).

### NEXT — cold-cache subtitle prefetch during search
First-play subtitle cost is the cold alass audio-VAD (~15s, now capped at 5s → raw sub fallback). Prefetch + align subtitles in the BACKGROUND when the user views search results (before they tap play) → by play-time the aligned SRT is cached → fast first-play WITH aligned subs. Decouples subtitle latency from the play entirely (for the DMR-burn-in path; the custom receiver makes it moot).

### NEXT: a shorter race-ahead lead for COMPLETE local files
Lower the race-ahead `MIN_LEAD_SECS` (120 s → about 40 s) when the source is a complete
local file. A 3.5× transcode of a complete file is never caught by playback, so 120 s of
pre-buffer only adds about 30 s to every start; a smaller lead would start a Chromecast
play in 15 to 20 s instead of about 50. (Split out of the *decouple `/play` from the HTTP
request* item, which shipped on 2026-10-03 as the play job.)

### samklang — torrent-streamed music extension (SPEC SHIPPED 2026-06-02, NOT YET IMPLEMENTED) 🎵
Three-file Kiro spec at `.claude/specs/samklang/{requirements,design,tasks}.md`. Music streaming for a small invited circle, built as `MediaType::Music` extension to spela (one binary; `/music/*` routes; new `/listen` tab in `static/remote.html`). Torrent-only streaming (BT primary v1, Soulseek + Internet Archive in phase-4 try-harder path), progressive in-time quality upgrade via Web Audio + MSE source-swap (mp3 → FLAC seamlessly), bearer-token multi-user, Postgres on the server :5433 for state. 5 phases ~5-6 weeks at AI-augmented velocity; **phase 1 weekend-shippable** (search + play + web /listen tab, single user, Jackett-on-the server-Docker as the indexer hub). Cross-cutting non-regression invariant: existing video paths (`MediaType::{Movie, TvShow}`) bit-identical throughout. Companion: dotfiles (B3) the renderer BT-audio side-quest must resolve before T-3.11 Marshall-via-the renderer target can land — Chromecast cast (T-3.7) ships first.

---

### Renderer (thin client): the owner's deployment work moved to private notes on 2026-11-04 (`CLAUDE.local.md` points to them). The server side of the renderer target is `renderer_watch_url` + `renderer_name` in `config.toml`.

### 🔓 OPEN — the owner's Mac-side library pieces: moved to private notes on 2026-11-04 (`CLAUDE.local.md`).

### 🔓 OPEN — HLS cache, two sizing decisions left

The cache is complete and live at 12 GiB (`Config.hls_cache_cap_mb`).

- [ ] **Raise the cap to 20480** (20 GB ≈ 60 episodes) once movie-night usage shows
  the extra room is wanted. 12288 was chosen conservatively for the server disk pressure;
  live-validate before flipping.
- [ ] **Background transcode-ahead** (deferred): when a watch stops at minute 5,
  finish transcoding the rest in the background to populate the cache. Adds an ffmpeg
  subprocess plus GPU contention, which is why it was deferred rather than built.

### Custom Cast Receiver scaffolds — NOT dead code (leave them)

`handle_seek_restart` (server.rs:5407) + `handle_retry` (server.rs:5536) look like dead stubs by their bodies (handle_retry just returns `{"status":"retry_requested"}` with a TODO). They are **NOT webtorrent-era cruft** — git blame shows BOTH shipped in `a9637a6 "feat: Custom Cast Receiver — intro, seeking, subs, resume, overlay"`. The receiver HTML at `static/cast-receiver.html` actively fetches both endpoints (lines 240 + 398). Deleting either would break the Custom Cast Receiver scaffolding. **They're awaiting wiring as part of the $5-Google-registration Custom Cast Receiver landing** (see "Still Open" → Custom Cast Receiver below). When that work lands, both handlers should be implemented properly: `handle_seek_restart` → call `do_play(seek_to=req.t)` (the same path v3.7.7's `/seek-retranscode` uses); `handle_retry` → call the existing `handle_play` auto-retry path.

### Deferred (audit items NOT actioned, with reasons)
- **H5** Cast-receiver IP allowlist — LAN-IP-config friction without much marginal value over iptables-INPUT-DROP + Host-header + LAN-trust layers already in place. User explicit decision.
- **M10** `do_cleanup` race lock — analyzed: librqbit's `session.delete` is idempotent on missing IDs; the race produces a spurious warning log but no actual leak. Adding sync primitives that aren't strictly needed is bad design.
- **L3-L8** polish items (urlencoded check, magnet logging audit, history date-cap, body-builder unwrap code-smell, CORS preflight, etc.) — low-impact; spot-checked during the H1 work, no concrete leaks.
- **Bearer-token auth** — would require plumbing through spela CLI + Ruby's `run_spela` tool. Future-session item if/when WAN exposure happens (Tailscale, port-forward).
- **Full cast_health_monitor `CastMonitorAction` extraction** — 200+ LOC interacting state across 8 mutable variables; high regression risk for a critical path. Sub-decision extraction captured the most-likely-to-drift constants. Future-session candidate.
- **Test-hygiene `tempfile::TempDir` migration for the existing fixed-label tempdirs** — process-id-namespaced fixed-label dirs are parallel-safe; only loss is RAII cleanup on panic, not a real bug we've hit.

### Corrupt-source-file detection (open) 🔧
- [x] **Auto-detect corrupt source files via ffmpeg.log post-mortem** (SHIPPED Apr 30, 2026): `transcode::inspect_ffmpeg_log_for_corruption` parses `~/.spela/ffmpeg.log` for three signals (EBML parse errors, missing HEVC reference frames, `dup=>100` on summary line). `do_cleanup` records corrupt source paths in `AppState.corrupt_files` HashSet; `find_local_bypass_match` skips entries on subsequent scans. 8 unit tests pin parser semantics + corrupt-skip behavior. First failed cast marks the file; manual `rm` no longer needed.

- [ ] ~~**Auto-detect corrupt source files via ffmpeg.log post-mortem**~~ — Apr 29 incident (Hijack S02E05 MeGusta) silently produced ~5 min of NVENC-duplicated junk past byte 417 MB Matroska corruption. Local Bypass treated the broken file as good on every subsequent play; recast loop wedged 5x at the same `time=2022s` before manual rm. Implementation: after each transcode_hls completion, parse `~/.spela/ffmpeg.log` for (a) `dup=<N>` on final summary line where N>100, (b) any `Could not find ref with POC` lines, (c) any `invalid as first byte of an EBML number` lines. If any match, mark the source file path in `state.json::corrupt_files` and have Local Bypass skip it on subsequent matches. Hard-won lesson context in [CLAUDE.md](CLAUDE.md) § "Corrupt source files defeat auto-recast — wedge isn't on the receiver".

### Still Open

- [ ] **Ruby's pre-confirmation speaks BEFORE the tool call completes** — `conversation_engine.py::_execute_spela` synthesizes "Got it — season X episode Y on your Chromecast" BEFORE running the spela subprocess, so when the subprocess fails (disk cap, dead seeds, etc.) Fredrik hears success but gets no correction. Apr 15 system-prompt update (dotfiles commit `494a82f`) told Gemini to speak an error correction as her first action on the next turn; validate that the fix actually works the next time a play fails. If Gemini still ignores tool errors, escalate to a code-level fix in `_execute_spela`: detect error responses and synthesize + play an error correction TTS BEFORE returning the tool result to Gemini. Don't depend on Gemini's discretion for critical error relay.

- [x] **spela search returns spurious non-matching shows (mitigated)** — the Apr 15 live incident where `The.French.Chef.Season.05.03of20.Queen.of.Sheba.Cake` was returned as a top-seeded candidate for a "The Boys" S05E03 query is now defended against by a post-fetch token filter (`filter_results_by_show_title` in `src/search.rs`, shipped v3.1.0 commit `33c5e5e`). 7 regression tests including the Apr 15 live fixture. **Root cause still unknown** — Torrentio's IMDb-ID-routed endpoint shouldn't return cross-show data, but it did. Left as a defense-in-depth open question: diagnose whether this is a Torrentio upstream bug, a TMDB multi-search mismatch resolving to the wrong IMDb ID, or a spela fallback path we didn't expect. Not blocking any user-facing flow.

- [ ] **Real release-name corpus test for the ranker** — the resolution-tier tests use live fixtures, but we still lack a broad corpus of ~50 real Torrentio titles to catch regressions in tier 1-3 logic with realistic naming variation. Add `tests/ranker_corpus.rs` that loads the fixture and asserts canonical ordering. Should include every Apr 15 regression case as a pin.


- [ ] **Custom Cast Receiver — Google Cast SDK $5 registration.** Fredrik offered to pay; not yet done. Registration unlocks the Custom Receiver App ID needed by `static/cast-receiver.html` (Shaka Player + CAF v3). Benefits: (1) **native seeking** — Shaka does HLS seeking by segment index, the current server-side `/api/seek-restart` restart-the-transcode-from-new-ss approach becomes unnecessary; (2) **text-track handling** — can send WebVTT subtitles separately via Cast protocol instead of burning into video, eliminating the subtitle shift dance in `transcode.rs::shift_srt`; (3) **loading state UI** — replaces the blue-cast-icon "Black Flash" between clips. Blocking step: register at `cast.google.com/publish`, pay $5, get Receiver App ID, set it in `~/.config/spela/config.toml`, switch `cast.rs::cast_url` to load the custom receiver instead of Default Media Receiver. Spec lives at `.claude/specs/custom-cast-receiver/`.

- [x] **Intro concat fix** (SHIPPED Apr 30, 2026 — awaiting live validation): four ffmpeg args added to the has_intro h264_nvenc branch in both `transcode()` and `transcode_hls()`. Original investigation note follows for context. **Validate by**: `mv ~/.config/spela/intro.mp4.disabled ~/.config/spela/intro.mp4` on the server, play any title, verify smooth playback past segment 5. Pre-commit verification recipe (per investigation note): `ssh <server> 'for i in 00000 00001 00004 00005 00006; do ffprobe -v error -select_streams v -show_frames -read_intervals "%+#1" ~/media/transcoded_hls/seg_${i}.ts | grep -E "key_frame|pict_type"; done'` — every segment's first frame must be `key_frame=1 pict_type=I`.

- [ ] ~~**Intro concat investigation**~~ — with `--no-intro`, HLS plays smoothly; WITH intro, Chromecast (CrKey 1.56) buffers indefinitely at ~segment 5 (30-36s into playback). **Hypothesis** (Apr 19 session): NVENC's scene-change detection fires an unscheduled IDR at the intro→main concat seam (t≈5s, a huge visual transition from the 5s intro clip to the movie opening). That desyncs NVENC's GOP cadence from the HLS segmenter's 6s clock. Segments 1-4 happen to land on keyframes by luck; by segment 5, accumulated cadence drift means either (a) the segment doesn't start with a keyframe, or (b) its actual duration disagrees with its EXTINF value by more than CrKey 1.56 Shaka Player's tolerance. The concat seam itself is inside segment 0 (0-6s = intro 0-5 + main 5-6), not at the segment-5 boundary — so the failure is a *downstream consequence* of the seam, not the seam directly. **Proposed fix** — add four args to both `transcode()` and `transcode_hls()` in `src/transcode.rs` (the filter_complex + nvenc branch when `has_intro`): `-g 180 -keyint_min 180 -sc_threshold 0 -force_key_frames expr:gte(t,n_forced*6)`. Fixed 6s GOP at 30fps, disables NVENC scene-change IDR injection, guarantees an IDR at every 6s boundary regardless of encoder cadence — the canonical ffmpeg+HLS incantation. **Cheap pre-commit verification**: after a with-intro play, `ssh <server> 'for i in 00000 00001 00004 00005 00006; do ffprobe -v error -select_streams v -show_frames -read_intervals "%+#1" ~/media/transcoded_hls/seg_${i}.ts | grep -E "key_frame|pict_type"; done'` — every segment's first frame must be `key_frame=1 pict_type=I`. If segment 5's first frame is `pict_type=P`, that's hypothesis confirmed. **Not urgent**: `--no-intro` workaround is solid and voice-assistant paths already pass it by default for resume/seek plays. Prioritize below user-facing regressions.

### Infrastructure Hardening
- The owner's router and DNS items moved to private notes on 2026-11-04 (`CLAUDE.local.md`).

### Post-Search Optimization
- [ ] Implement Chromecast Receiver UI loading state (spinner/progress) to replace the "Black Flash" transition between video clips. 🎉🏙️
- [ ] Refine year-aware and quality-aware result prioritisation in `src/search.rs` to better handle franchise sequels (e.g., 2025 vs 2026). 🕵️‍♂️
- [ ] **TVTime "next unwatched episode" integration for Ruby** — Ruby should know which episodes the user has actually watched before guessing from search ranking. Tonight's failure mode: when user said "play the Boys episode" with no season/episode, Ruby chose the latest available (S05E02 — unwatched) instead of the next unwatched (S05E01). Architecture sketched in `~/.claude/hooks/conversation_engine.py` near `SPELA_TOOL_DECLARATION`: new `tvtime_client.py` with `get_show_progress(show)` + `mark_watched(show, s, e)`, new Gemini tool `get_next_episode(show)` registered alongside `run_spela`, system-prompt rule "before run_spela play on a TV show, always call get_next_episode unless the user explicitly named the season/episode", read-through 1h cache invalidated on every successful spela play of a TV show. Auth: TVTime removed their public API ~2019; reverse-engineered sidecar (`https://beta.tvtime.com/sidecar?o=https://api.tvtime.com/v1/...`) is the only stable route. Trakt.tv has an official API and many TVTime users sync to it — prefer Trakt if available. Currently blocked on user finding working credentials (Apr 15 reset-password-hell). Cross-reference: dotfiles commit `57b2d92`.

### System Hardening
- [x] Increase Node.js heap memory limits by default for webtorrent-cli to prevent "Ghost Crashes" during large file verification. Implemented in `src/torrent.rs` with `--max-old-space-size=4096`.
- [x] Add robust stale WebTorrent worker detection to prevent orphan workers from inflating disk, RAM, and swap usage. See `OPERATIONS.md`.
- [x] Add a worker-only cleanup command/path that terminates WebTorrent/ffmpeg without deleting media or rewriting playback history. Implemented as local `spela kill-workers`.
- [x] Reconcile stale WebTorrent workers on server startup before accepting new playback.
- [x] Add systemd/cgroup containment for media workers (`MemoryHigh`, `MemoryMax`, task limits, and CPU quota/weight where practical). Implemented as `systemd/spela-resource-limits.conf`.
- [x] Tighten local-bypass completion markers so `.spela_done` is written only when expected torrent byte size and physical file bytes agree.
- [ ] Add an external watchdog/alert that reports orphan WebTorrent workers and resource pressure before the host becomes unhealthy.

### Subtitle source priority — prefer the torrent's BUNDLED sidecar .srt over OpenSubtitles
The VLC sub path (`handle_vlc_sub`) always fetches OpenSubtitles, which is often NOT synced to the specific rip → misaligned (T2 2026-08-14: OpenSubtitles srt drifted; the YIFY release's own sibling `.srt` was perfectly synced by construction). Fix: detect a sidecar `.srt` next to the video — either already on disk (`~/media/<release>/<name>.srt`) or by selecting+downloading its file index in the torrent (currently `only_files` selects the video index only). Priority order should be: embedded text track (already done) → bundled sidecar `.srt` (release-matched) → OpenSubtitles + alass align. Applies to the VLC path AND any future cast/browser sub path. Bonus: also honour multiple bundled langs (pick original-language per the never-a-dub rule).


