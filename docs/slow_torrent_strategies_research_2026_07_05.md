# Slow/obscure-torrent streaming strategies — research (2026-07-05)

Two parallel research agents on how to make obscure/slow torrents (few seeds,
slow unchoke: peers connect but deliver 0 bytes for 10-15s) stream fast +
reliably, for spela (self-hosted Rust/librqbit torrent→Chromecast, home
connection, single GPU). Reports persisted verbatim below.

## Synthesis / decisions (Fredrik-facing)

- **Cheap engine-tuning floor (Layer-2, no decision needed, testable now):**
  fat public-tracker injection (ngosang/trackerslist, spela currently appends
  only 5) + librqbit connection/announce tuning + force-reannounce on stall.
  Shaves the 50-90s slow-unchoke on *alive-but-thin* swarms; zero help on dead
  swarms. **Check the anacrolix `MaxUnverifiedBytes=0` gotcha** — "peers
  connected, 0 bytes, frozen" is spela's EXACT observed symptom; if librqbit
  has an analog knob it may be a direct fix.
- **The big win (Layer-1, FREDRIK DECISION — adds a ~$3/mo paid dependency +
  a new source):** a **debrid fallback (TorBox primary)**. Try the local swarm
  first; if no byte-0 in N s, fall to TorBox — cached → instant HTTPS, uncached
  obscure → datacenter fetch that beats the home swarm, streamed live via the
  existing `/progress` screen. ~200 lines of reqwest, config-gated (absent
  token ⇒ behaves exactly as today, zero regression). This converts the worst
  case (obscure film times out) into "instant or datacenter-fetch". **Racing**
  is best as local-swarm-vs-debrid (the `tordis` pattern), not swarm-vs-swarm.
- **Usenet (TorBox Pro) is the only cure for the 0-seed dead tail** — optional
  phase 2.

The engine-tuning floor is mine to implement + test tonight. The debrid
fallback is designed but gated on Fredrik's yes (paid + new source + secret).

---

## Report A — Product/community strategies (debrid + racing)

I have comprehensive material. Let me write the full report now.

---

# Making SLOW / OBSCURE torrents stream fast + reliably — Product/Community Strategy Report

**For:** a self-hosted Rust (`librqbit`) torrent-to-Chromecast tool on a home connection + single consumer GPU
**Problem being solved:** obscure films take **50–90 s to first byte** (slow peer unchoke) or **fail entirely**, forcing retry cascades. Root cause is that a home connection joining a **weak swarm** (few seeds, intermittent peers, sometimes seeders that connect for 1 s and transfer nothing) is bandwidth- and latency-bound by the *worst* link in the chain — the source's willingness to unchoke *you*.

---

## TL;DR — Ranked recommendation

| Rank | Strategy | Impact on the 50-90s-or-fail obscure problem | Feasibility for a librqbit tool | Verdict |
|---|---|---|---|---|
| **1** | **Debrid fallback (TorBox primary)** — try local torrent first, fall to debrid if no byte-0 in N s | **Decisive.** For *cached* content → near-instant HTTP. For *uncached* obscure → their datacenter + huge peer reach downloads it far faster than your home swarm, then serves HTTP | **High** — pure HTTP/REST, no torrent internals; ~200 lines | **DO THIS** |
| **2** | **librqbit tuning + tracker/DHT boosting** — raise connection caps, `announce_to_all_trackers/tiers`, inject a public-tracker list, connect-boost, keep-running | Moderate on *slow-but-alive* swarms; **zero** help on *dead* swarms | **High** — config on the existing `Session` | Cheap, do it anyway as the fast-path floor |
| **3** | **Multi-source racing** (race N releases of the same film; commit to first with byte-0) | Moderate-High for the "one of these releases has a live seed" case; the *established* bounded pattern is racing the debrid CDN vs the swarm (`tordis`), not swarm-vs-swarm | **Medium** — orchestration + wasted-bandwidth control | Best combined with #1 as the race target |
| **4** | **Usenet backend (via TorBox Pro or standalone)** | **Decisive for the "no seeds at all / very old / niche" tail** — Usenet has ~5yr retention, no seeder dependency, 800 MB/s | Medium (NZB indexer + the same debrid HTTP-out) | The real cure for the truly-dead-swarm tail |

**The single highest-leverage move:** wire a **debrid fallback with TorBox as primary**. It converts your worst case ("obscure film, 3 seeds, times out") into either an instant HTTP stream (cached) or a *datacenter-speed* fetch (uncached) that beats your home swarm — while keeping your existing librqbit path as the fast, private, zero-cost happy path for well-seeded content.

---

## 1. Debrid services — the #1 community answer, in depth

### 1.1 What a debrid actually is (mechanism)

A debrid is a paid middleman with datacenter bandwidth + large peer reach. You hand it a magnet; it joins the swarm *on its own high-bandwidth servers*, downloads the file to its storage at datacenter speeds, and hands you back a **direct HTTPS link** to stream or download. You watch from *their* server, not from the swarm.

> "A debrid service is a paid middleman between you and torrent-based streaming sources. Instead of your app connecting straight to a peer-to-peer swarm, the debrid service grabs the file on its own server and gives you a direct HTTPS stream. You watch from that server, not from a pile of strangers' computers." — prerolls.me

> "It downloads the file to its own storage at data-center speeds. Once the file is on that server, the service gives you a direct HTTPS link to stream or download it." — prerolls.me

Two outcome classes, and **this distinction is the whole game for your obscure-film problem**:

- **Cached hit** — someone (anyone across the whole platform) already fetched this exact infohash; the file lives on their servers. Your request returns a direct HTTPS URL that streams **instantly (1–3 s to first frame)**. No download slot consumed. *(smarttvs.org measured 18 s uncached → 2 s cached on the same release/network.)*
- **Uncached miss** — nobody has fetched it. You submit the magnet; *their servers* now join the swarm and pull the file. **The critical question for you:** how fast is their fetch vs. your home swarm?

### 1.2 THE critical question — uncached obscure download: datacenter fetch vs. your home swarm

This is the crux, and the answer is favorable but honest about the tail:

**For a torrent that has *some* live seeds** (your "50–90s slow unchoke" case): the debrid wins substantially. It has (a) datacenter bandwidth (not your home upload/asymmetry), (b) a persistent, port-open, high-reputation peer that seeders readily unchoke, and (c) it doesn't share *your* thin unchoke slot. TorBox's own knowledge base and community reports describe the uncached-fetch wait as **"a few minutes"** for niche content, then it's cached forever:

> "For niche content, TorBox may need to fetch and cache it first — this can take a few minutes." — arnav.au

> "When Torrentio surfaces a release Real-Debrid hasn't cached yet, RD has to pull the file from the BitTorrent swarm before it can stream it to you. That first-watch handshake adds a noticeable delay." — smarttvs.org

TorBox's Stremio addon exposes a **"Cache & Play"** mode that *waits up to 90 s* for an uncached fetch and then streams — meaning for a large fraction of uncached-but-alive torrents, the datacenter completes enough to start streaming inside 90 s:

> "The Stremio addon will only wait for up to 90 seconds for the video to download, otherwise it will return the common 'TorBox is downloading' video… This works with torrents as well, but Usenet downloads much faster." — TorBox changelog v8.1

Crucially: the debrid **is bound by the same seeder availability you are** — it is not magic. The Stremio FAQ is explicit:

> "For a torrent to become cached, the debrid service connects to the torrent network and downloads the torrent file to their servers. This process is dependent on the number and quality of seeders available for that torrent." — Viren070's guide

**So the honest model is:**
- **Dead swarm (0 real seeds):** debrid can't fetch it either. This is where **Usenet** (§4) is the only cure.
- **Alive-but-thin swarm (your 3-seed, 50-90s case):** debrid's datacenter + peer reach reliably beats your home line; typically seconds-to-a-few-minutes, and then permanently cached for all future plays.
- **Cached:** instant HTTP.

### 1.3 TorBox — recommended primary (modern REST API, best 2026 posture)

**Why TorBox first (2026 community consensus):**

> "For most users in 2026: TorBox is the better default choice… its cache now matches Real-Debrid for mainstream recent releases." — iptvranking

> "TorBox's cache coverage is reported as essentially complete by the streaming community [for anything from the last ~5 years]… The transparent API also means addons can query cache status accurately." — arnav.au

Three decisive advantages over Real-Debrid **for a self-hoster specifically**:
1. **It still has a working, accurate cache-check endpoint** (`/checkcached`) — Real-Debrid *killed* theirs (see §1.4). This lets you know *before committing* whether it'll be instant.
2. **Multi-IP, no IP-binding ban** (RD bans on concurrent-IP/VPN use). A home server that also serves other devices won't trip it.
3. **First-class cloud-torrenting model** + optional Usenet on Pro — you can push an uncached magnet and it fetches it for you (that's literally your fallback).

**Caveat:** TorBox's cache is *shallower for very old / very obscure / foreign-language* content than Real-Debrid's 15-year cache. That's the one axis where RD still wins — and it's exactly your obscure-film use case. **Mitigation:** for uncached obscure, TorBox will still *fetch* it (that's the point of the fallback), and Pro's Usenet covers the dead-swarm tail. Some power users run **TorBox primary + RD fallback** for maximum cache coverage (~$6-7/mo combined).

**Pricing (2026):**
| Plan | Price | Slots | Notes |
|---|---|---|---|
| Free | $0 | 1 | 10 GB max file. Real testing tier, no card. Also a **$1 1-day Pro trial** + a **7-day free trial each month** |
| **Essential** | **~$3/mo** | 3 | 200 GB max. **Unlimited cached streaming.** Recommended for your use |
| Standard | ~$5/mo | 5 | 14-day seed |
| Pro | ~$10/mo | 10 | 1 TB max, **Usenet access** (the dead-swarm cure) |

> "Slots are only used if you have TorBox download something for you that isn't cached yet. This means streaming isn't limited by them." — troypointinsider

So 3 slots (Essential) = 3 concurrent *uncached fetches*; cached plays are unlimited and consume no slot. For a single-household streamer, Essential is plenty.

**Rate limits (matter for your integration):**
- Global: **300 req/min** per token.
- `POST /torrents/createtorrent`: **60 uncached torrents/hour** (separate stricter bucket) + counts against the 300/min. Cached adds are only capped by 300/min.
- `/checkcached`: "<1 second per 100 hashes", cached 1 hr server-side.

**TorBox REST API — the exact self-hoster flow (magnet-in → HTTP-URL-out):**

Base: `https://api.torbox.app`, version `v1`. Auth: `Authorization: Bearer <API_KEY>`. Standard response envelope: `{success, error, detail, data}` — always branch on `success`.

```
1. (Optional, do first) CHECK CACHE
   GET  /v1/api/torrents/checkcached?hash=<INFOHASH>&format=object&list_files=true
   → data[<hash>] present  ⇒ cached (will be instant)
   → empty                 ⇒ uncached (will need a fetch)
   (POST /checkcached with {"hashes":[...]} for bulk)

2. ADD THE TORRENT
   POST /v1/api/torrents/createtorrent   (multipart/form-data)
     magnet = <magnet URI>
     seed   = 3            (don't seed — you're just fetching)
     # add_only_if_cached=true  ⇒ ONLY adds if cached (use to gate the "instant" path)
   → data: { hash, torrent_id, auth_id }
   # Use asynccreatetorrent for a fire-and-forget variant that returns instantly.

3. POLL FOR READY
   GET  /v1/api/torrents/mylist?id=<torrent_id>&bypass_cache=true
   → data.download_state ∈ {downloading, stalled (no seeds), metaDL, cached, completed, ...}
   → data.download_present == true  AND/OR  download_finished == true ⇒ ready
   → data.progress, seeds, peers, download_speed, eta   ← surface these to your warmup UI!
   # mylist is only refreshed every ~600s unless you pass bypass_cache=true.

4. REQUEST THE STREAM URL
   GET  /v1/api/torrents/requestdl?token=<API_KEY>&torrent_id=<id>&file_id=<file_id>&redirect=true
   → data: "https://store-xx.torbox.app/.../file.mkv"   (a CDN link, valid ~3h to START; unlimited once started)
   # ?redirect=true makes it a permalink you can hand straight to ffmpeg/Chromecast.
```

The returned CDN URL is a plain HTTPS file with **Range support** — it drops straight into your existing HLS-transcode pipeline (feed it to ffmpeg exactly where you'd feed a local file / torrent-stream), or, for a same-codec file, hand it to the Chromecast directly. The `download_state` values map cleanly onto your live `/progress` warmup screen (`stalled (no seeds)` is your explicit "this one's dead, fall through" signal).

Official SDKs exist (Python `torbox-sdk-py`, JS `torbox-sdk-js`, Go) if you want a reference, though the raw REST is trivial from Rust `reqwest`.

### 1.4 Real-Debrid — deepest cache, but a degraded self-hoster experience in 2026

**Mechanism identical** (`addMagnet → selectFiles → torrents/info → unrestrict/link`), and RD still has the **deepest historical cache** — the best odds that a genuinely obscure/old film is *already cached*:

> "Real-Debrid's single biggest technical advantage is the size of its cached torrent library. Because it has operated since 2009… the probability of any popular title already being cached is extremely high, including older content from the 1970s through to niche foreign-language releases that smaller services have not yet cached." — arnav.au

**But three 2026 problems make it a worse *primary* for a self-hosted tool:**

1. **`instantAvailability` cache-check endpoint is DEAD (killed Nov 2024).** You can no longer ask "is this cached?" It now returns `{"error":"disabled_endpoint","error_code":37}`.
   > "They've killed the instantAvailability endpoint." — Debrid Media Manager
   > "GET /torrents/instantAvailability/{infohash} → {\"error\": \"disabled_endpoint\", \"error_code\": 37}" — riven #1394

   **Consequence for you:** the *only* way to know if RD has it is to **add the magnet, select files, and see if it flips to `downloaded` (cached) or starts `downloading` (uncached)** — a stateful, side-effecting probe:
   > "In Real-Debrid, after you've added a torrent (and selected files), if its status becomes `downloaded` after this, then it's cached. If not, it's not cached." — DMM FAQ

   This is exactly why the DebriDav project notes it must *start then immediately delete* the torrent to test availability.

2. **May 2026 keyword filtering** removed a large slice of the cache (blocks WEB-DL, WEBRip, AMZN, NF, CR filename tags) — "50-70% of cached content blocked as of June 2026" per one comparison. Sources that used to resolve now come back *empty*.

3. **Strict 1-IP binding** — bans on concurrent-IP / VPN use. Awkward for a home server serving multiple household devices.

**RD flow (for completeness / as a *secondary* fallback):**
```
POST /rest/1.0/torrents/addMagnet   magnet=<uri>            → {id}
POST /rest/1.0/torrents/selectFiles/{id}   files=all|<ids>
GET  /rest/1.0/torrents/info/{id}   → status ∈ {downloading, downloaded, ...}; poll until downloaded
POST /rest/1.0/unrestrict/link      link=<hoster link from info.links[]>  → {download: "https://...", streamable}
```

**Net RD verdict:** keep it as an *optional secondary* fallback specifically for the *deep obscure/old catalog* where its cache depth beats TorBox — but not as primary, because the dead cache-check + IP binding + filtering make it clumsy to self-host in 2026.

### 1.5 The others (brief)

| Service | 2026 price | Torrent cache | Cache-check API | Self-hoster notes |
|---|---|---|---|---|
| **AllDebrid** | ~€2.99/mo (7-day free trial) | Growing, ~79% hit in a 50-title test | Restricted (like RD) | Cheapest, closest RD drop-in. Clean CLI/SDK ecosystem (`adb magnet upload → watch → files`). `magnet.upload → magnet.status/watch → magnet.files` |
| **Premiumize** | ~$9.99/mo | Good | ✅ has cache-check + **1 TB cloud + VPN + Usenet** | Priciest; but the bundled VPN+cloud+Usenet can consolidate spend. `/cache/check` works |
| **Debrid-Link** | ~€3-4/mo | Smaller | Restricted | Minor player; no strong reason over TorBox |

**AllDebrid API shape** (very close to RD): `magnet/upload` → poll `magnet/status` (or `magnet watch`) → `magnet/files` returns direct links. Statuses: `active`(downloading) / `ready`(done) / `expired` / `error`.

---

## 2. How the streaming community actually solves this

The near-universal 2026 stack: **Stremio** + a debrid-aware addon (**Torrentio**, **Comet**, **MediaFusion**, **AIOStreams**) + a **debrid backend**. The addon scrapes torrent indexers, and for each result **asks the debrid whether it's cached**, showing `[RD+]` / `[TB+]` (cached, instant) sources first.

> "In Stremio, add-ons like Torrentio check what is cached in your debrid service and show those links first — marked as instant — so you know they will play without waiting for a download." — iptvranking

**Why debrid is the near-universal recommendation for reliability + obscure content** (verbatim community reasoning):

> "Torrenting relies on seeders being available constantly and you are limited by the upload speed of the seeders. With a Debrid Service, this issue is solved as the torrent is downloaded to the Debrid services' high speed servers… Once RD has the file, seeder count stops mattering, which is why cached RD sources are so much more reliable than uncached." — Viren070 / smarttvs.org

The addon architecture you should copy (from **Sootio**, a debrid search engine): scrape *multiple* sources **in parallel**, group/rank by quality tier, **run cache-checks tier-by-tier highest-quality-first, and early-exit** the moment enough cached top-tier results are found:

> "Scrape All Sources → Group & Rank → Process in Tiers (cache checks highest-quality first) → Early Exit: immediately returns top-quality results once thresholds are met." — sooti/sootio README

This is directly transplantable to your ranker: after your existing Sarpetorp 5-tier rank, **batch-`checkcached` the top N hashes on TorBox** and prefer a cached result even if it's a slightly lower codec tier — instant-play usually beats a marginally better encode that needs a 60 s uncached fetch.

**The uncached one-click UX pattern** (what you should build): when the user picks something uncached, most addons play a 10 s "added to your debrid, try again later" stub. The community's stated desire — and your opportunity — is the seamless version:

> "It would be much more convenient if simply clicking the stream automatically added the uncached torrent to the debrid service in the background. A true one-click solution." — Stremio-StashDB-Debrid #2

Your **live `/progress` warmup screen already does exactly this** — poll the debrid's `mylist` `progress/seeds/eta` and show it, then swap to the CDN URL when `download_present` flips true. You're architecturally ahead here.

**Non-debrid community tricks** (all weaker than debrid, but real):
- **Prefer 1080p over 4K** — the 1080p cache pool / swarm is "5-10× larger", dramatically better cached-odds and lighter transcode (you already demote 2160p — reinforced).
- **Sort by Quality *then* Seeders** and filter to cached-only where possible.
- **Time-of-day**: niche swarms thin at night ("200 seeders at noon → 40 at midnight"). Not actionable for a controller, but explains variance.
- **Usenet** as the structural fix for dead swarms (§4).

---

## 3. Racing multiple torrent sources in parallel

### 3.1 The established, working pattern: race the debrid CDN vs. the public swarm

The most directly relevant prior art is **`tordis`** (a headless Go torrent engine with an HTTP API — architecturally *your project's cousin*, created July 2026). Its headline feature is exactly a bounded race:

> "**Optional Real-Debrid race** — with a token, tordis races RD's cached CDN against the public swarm and returns whichever wins." — tordis README

This is the pattern to adopt: **don't race swarm-vs-swarm; race your-local-swarm vs. the-debrid.** It sidesteps the wasted-bandwidth and piece-duplication problems (§3.3) because the two racers download *different byte streams* (P2P pieces vs. an HTTP file) and you simply take whichever produces a playable byte-0 first. `tordis` also demonstrates the exact API surface you already have (`POST /download {magnet}` → poll `/status` → `GET /file/{id}/video` with Range), confirming your design is idiomatic.

Other multi-engine prior art:
- **PlayTorrio** runs "up to 3 engine instances simultaneously" for "better swarm participation" and "predictive pre-warming: cache first segments for instant playback" — but note this is *3 instances of the same torrent for more peers*, not 3 different releases.

### 3.2 Racing multiple *releases* of the same film (swarm-vs-swarm)

This is a legitimate but bandwidth-costly pattern. The clean bounded form: **start the top 2-3 ranked releases, request only the first pieces (byte-0 head) of each, and commit to the first one that delivers a playable head; cancel the rest.** Because you only fetch the *head* of the losers before cancelling, wasted data is bounded to a few MB per loser rather than a full duplicate download.

librqbit supports the primitives: `only_files` file selection + you can prioritize/stream the head via `FileStream`, and `stop()` to kill losers. Your existing `race_ahead_safe` gate is conceptually the single-source version of this.

**Recommended bounded pattern:** `race_head(sources[0..3], head_bytes=first_2_segments, deadline=Ns)` → first to deliver a hashed, playable head wins → `stop()` the others → continue only the winner. Combine with the debrid as a *fourth racer* (per `tordis`) so a dead-swarm trio still loses to the datacenter fetch.

### 3.3 Tradeoffs & the piece-duplication hazard (be careful)

- **Bandwidth split**: N parallel full-downloads split your home downlink N ways — counterproductive if all N are thin. **This is why you bound the race to the head, not the whole file.**
- **Piece duplication across swarms** (arvidn/libtorrent #829, the canonical warning): two swarms sharing the same file can make you download *the same piece twice* from the same physically-shared peer, "wasting 50% of your download." Racing *different releases* (different infohashes/piece layouts) mostly avoids this, but racing the *same* infohash across engines does not — so **race different releases, or race swarm-vs-debrid, never the same infohash across two torrent engines.**
- **Transcode cost**: don't start your single-GPU NVENC transcode until a winner is chosen — race at the *download* layer only, then transcode the winner. (You already gate transcode behind the cast decision, so this fits.)

**Verdict:** the highest-value race is **local-swarm vs. TorBox** (à la `tordis`), with an optional **head-race across your top 2-3 ranked releases** as the swarm-side entrant. Pure swarm-vs-swarm full-download racing is not worth the bandwidth/duplication cost on a home line.

---

## 4. Usenet — the real cure for the truly-dead-swarm tail (bonus, high-value)

For the obscure films with **0 real seeds** (where neither your engine nor a debrid can help — "seeder connects for 1 s, transfers nothing"), Usenet is the structural answer: article-based, **no seeder dependency**, ~5-year retention, ~800 MB/s on TorBox.

> "Usenet is a separate file-sharing network largely immune to DMCA takedowns — content stays available long after it disappears from torrent sources. For very old films, niche content, or non-English content without strong torrent availability, Usenet fills the gap." — iptvranking

> "TorBox Pro effectively combines a debrid service and a Usenet provider into one subscription." — iptvranking

**TorBox Pro ($10/mo)** bundles Usenet + the same HTTP-out API, so your fallback ladder can become: **local torrent → TorBox cached → TorBox torrent-fetch → TorBox Usenet (via an NZB indexer)** — the last rung catching the dead-swarm tail your current tool can never serve. This is optional/phase-2 but it's the *complete* answer to "obscure film, zero seeds."

---

## 5. librqbit / swarm-tuning floor (cheap, do regardless — improves the happy path)

These won't save a dead swarm, but they shave your **50-90s slow-unchoke** on *alive-but-thin* swarms and cost almost nothing. From libtorrent's tuning guide + anacrolix/torrent field reports (the engine `tordis` uses); librqbit exposes analogous knobs via `SessionOptions`/per-torrent options:

- **Raise connection limits & keep upload unthrottled.** `connections_limit` high, `upload_rate_limit = 0` (infinite). More candidate peers = faster chance one unchokes you.
- **Announce aggressively.** `announce_to_all_trackers = true`, `announce_to_all_tiers = true` — hit *every* tracker at once instead of tier-by-tier, maximizing peer discovery for thin swarms.
- **Inject a fat public-tracker list** into each magnet before adding (the community's #1 slow-torrent trick). A magnet with 3 trackers becomes one with 40; more trackers = more peer sources. Maintain a current public-tracker list (e.g. ngosang/trackerslist) and append its `&tr=` params.
- **DHT + PEX on, `torrent_connect_boost`** (libtorrent defaults ~30 initial parallel connects) — connect to many peers immediately on add rather than trickling.
- **`AlwaysWantConns = true` / keep the torrent actively wanting connections** (anacrolix field-tested config for slow swarms), and **`predictive_piece_announce`** to shave ~1.5 RTT/piece.
- **Force-reannounce on stall.** If no byte-0 in ~10-15 s, re-announce to trackers + DHT to pull a fresh peer set (mirrors TorBox's own `reannounce` control op).
- **First-and-last-piece + sequential head priority** for instant playability — you effectively do this via `FileStream`/race-ahead already; keep it.
- **Run persistently.** For a lone-seeder obscure torrent, the community answer is literally "run your client 24/7" — a long-lived librqbit session that keeps the magnet warm means the *next* play of that film is instant from your own disk (which your Local Bypass already exploits).
- **anacrolix gotcha worth knowing** (may have a librqbit analog): with many rapidly-downloading peers + a `MaxUnverifiedBytes` cap, a zero-availability-piece sort bug can *freeze* progress until enough pieces appear; the workaround was `MaxUnverifiedBytes = 0`. If librqbit ever "stalls at 0% with peers connected," check the equivalent unverified-buffer setting.

---

## 6. Concrete integration sketch for spela (the fallback ladder)

Fits your existing `do_play` + `/progress` warmup architecture with minimal surface:

```
handle_play(result):
  # Rung 0 — you already have this
  if local_bypass_match(result): serve local file  ✓ instant

  # Rung 1 — start the local swarm AND (optionally) query debrid cache in parallel
  torrent_handle = TorrentEngine::start(magnet)          # your existing path
  if TORBOX_TOKEN set:
      cached = torbox.checkcached(infohash)              # <1s, non-committal

  # Rung 2 — if debrid says CACHED, race it against the swarm (tordis pattern)
  if cached:
      spawn: torbox.add(magnet, add_only_if_cached=true)
              -> requestdl -> HTTPS CDN url                # ~1-3s to a playable URL
      # race: whichever of {torrent byte-0, torbox CDN byte-0} is ready first wins
      winner = select(torrent_head_ready, torbox_url_ready, deadline=Ns)

  # Rung 3 — swarm fail-fast → debrid uncached fetch (your 50-90s-or-fail cure)
  if no torrent byte-0 within N seconds (your should_fail_fast_stream_start):
      if TORBOX_TOKEN:
          id = torbox.createtorrent(magnet, seed=3)        # datacenter joins the swarm
          begin_warmup(source="torbox")                    # reuse your /progress screen!
          poll torbox.mylist(id, bypass_cache=true):
              publish progress/seeds/eta to /progress       # live UI, already built
              on stalled(no seeds): break -> Rung 4 or next ranked release
              on download_present: url = torbox.requestdl(id, file_id, redirect=true)
          feed `url` into your normal HLS transcode / cast pipeline
      else:
          auto-retry next ranked release (your current behavior)

  # Rung 4 (phase 2, optional) — dead swarm → Usenet via TorBox Pro + NZB indexer
```

**Design notes matched to your directives:**
- **Config-gated, reversible:** `torbox_token` in `~/.config/spela/config.toml`; absent ⇒ behaves exactly as today (pure librqbit). Zero regression risk — a clean *Architecture vs parameters* Layer-1 addition.
- **`N` (fail-fast deadline)** is a Layer-3 parameter — start at your existing 20 s `should_fail_fast_stream_start`, tune from observation.
- **Reuse `/progress`:** debrid `mylist` (`progress`, `seeds`, `peers`, `download_speed`, `eta`, `download_state`) maps 1:1 onto your existing warmup phases — the `stalled (no seeds)` state is your explicit "this rung is dead, fall through" oracle (better than a blind timeout).
- **SSRF discipline:** the CDN URL comes from TorBox's domain — validate it's an expected `*.torbox.app` / `store-*.torbox.app` host at your HTTP boundary before feeding ffmpeg, consistent with your existing `validate_magnet_uri` boundary-allowlist pattern.
- **Secrets:** `TORBOX_API_KEY` belongs in a tier secret (`tier-edge`/`tier-servers`), never in the public repo — mirrors your existing config hygiene.
- **Prefer-cached ranker tweak:** after your 5-tier rank, batch-`checkcached` the top N hashes; a cached tier-4 result should usually beat an uncached tier-1 (instant-play > marginally-better-encode-that-times-out). This is the Sootio "process tiers, early-exit on cached" pattern.

**Cost/benefit:** ~$3/mo (TorBox Essential) + ~200 lines of `reqwest` HTTP converts your worst failure mode (obscure film times out, retry cascade, user waits and often gives up) into "instant if cached, datacenter-fetch-with-live-progress if not, and the swarm still wins for well-seeded content." For a *quality > dev-effort* value hierarchy on a home line with a single GPU, this is the highest-ROI reliability investment available.

---

## Sources (Report A)

**Debrid mechanics / community:** prerolls.me; guides.viren070.me; iptvranking; smarttvs.org; StreamStack; sooti/sootio README. **TorBox API:** api.torbox.app/docs; postman.com/torbox; TorBox changelog v8.1/v5.0; TorBox-App SDKs. **TorBox vs RD 2026:** geekextreme.com; stremioguide.com; arnav.au; ElfHosted; troypointinsider. **Real-Debrid API + instantAvailability death:** api.real-debrid.com; valentingot.github.io/real-debrid; Debrid Media Manager; rogerfar/rdt-client #545/#617; rivenmedia/riven #1394; firestick.io. **Others:** fynks/debrid-services-comparison; debridcompare.xyz; iptvwire.org; @adbjs/cli & sdk; skjaere/DebriDav. **Racing:** iWebbIO/tordis; ayman708-UX/PlayTorrio; arvidn/libtorrent #829; patricker/TorrentBalancer. **Swarm tuning:** libtorrent.org/tuning.html; anacrolix/torrent #953 & #916; alt-torrent.com; bitstorrent.com; feddit.dk.

---

## Report B — Engine-internal (librqbit piece-selection + swarm-widening)

Full report persisted verbatim at
`docs/librqbit_streaming_faststart_research_2026_07_05.md`. TL;DR:

- **spela already has librqbit's streaming prioritization** — a 32 MB read-head
  window feeds the picker, files pick first+last+mid (moov early), and there's
  a steal-from-slow-peer mechanism. The transcoder "stall" is *correct
  back-pressure* (poll_read Pending + waker), so the real fix is **peer supply**,
  not prioritization.
- **The first few pieces have the weakest anti-stall protection** (`try_steal`
  returns None when `peer_avg_time` is None at t=0) — exactly the 10-15s
  slow-unchoke window. Fix = supply more/faster peers.
- **Win A (SHIPPED): session-level tracker injection.** `AddTorrentOptions.trackers`
  is silently dropped on the magnet path (spela uses magnets); `SessionOptions.trackers`
  is the reliable path (merged into every torrent, respects the private flag).
  Expanded 5 → 20 (ngosang best).
- **Win D (needs Fredrik): enable the inbound TCP/uTP listener** (`listen: None`
  by default). ~doubles reachable peers, but needs a WAN inbound port-forward on
  the Darwin router (nftables) — a security/shared-infra decision, deferred.
- DHT + PEX already on. Web seeds (BEP-19) not supported by librqbit (skip).
