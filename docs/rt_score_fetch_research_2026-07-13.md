# Rotten Tomatoes data fetch — robustness research (2026-07-13)

Research question: the MOST ROBUST way, as of 2026, to programmatically obtain **Rotten Tomatoes data** (critic Tomatometer %, audience/Popcornmeter %, critics-consensus prose, a reliable RT deep link) for **movies AND TV series**, for a self-hosted, cacheable, low-maintenance personal media app (spela). All facts below verified against 2026 primary/authoritative sources (URLs inline). Training data on RT's partner API + page structure is stale — this doc supersedes it.

---

## TL;DR ranked recommendation

**Primary: MDBList API** (`api.mdblist.com`, free API key, 1000 req/day).
- Returns RT **critic (`tomatoes`)** + **audience (`popcorn`)** scores for BOTH movies AND TV series, normalized 0–100, plus a per-source `url` (the deterministic RT deep link — solves "read more →" cleanly).
- Free, low-maintenance (a maintained aggregator others already depend on — Kometa/Plex, Jellyfin plugins, Kodi TMDb-helper), cache-to-disk friendly, keyed by IMDb OR TMDB ID (spela already has both).
- **Does NOT carry critics-consensus prose.** That is the one honest gap.

**Fallback / consensus-prose source: OMDb API** (`omdbapi.com`, free key, 1000 req/day).
- For **MOVIES**: `Ratings[]` includes `{"Source":"Rotten Tomatoes","Value":"NN%"}` AND a `tomatoConsensus` field (with `&tomatoes=true`) — the consensus prose.
- For **TV SERIES**: **NO RT data at all** — RT score N/A, `tomatoMeter`/`tomatoConsensus` all `N/A` (verified live below). This gap is long-standing and STILL real in 2026.

**Honest consensus-prose verdict:** the RT critic SCORE is cleanly available for movies+TV via MDBList. The critics-**CONSENSUS PROSE** is only cleanly available for **movies** (via OMDb `tomatoConsensus`); for **TV series** there is **no clean free structured source** — the only ways to get TV consensus prose are (a) direct RT page scraping (`id="critics-consensus"` / JSON-LD / media-scorecard blob — all in initial SSR HTML), or (b) a paid RapidAPI/Apify scraper wrapper. Both are RT-ToS-violating and higher-maintenance. **Recommendation: treat consensus prose as movie-only best-effort via OMDb, and don't block the feature on TV consensus.**

---

## Per-path findings (current 2026 status)

### 1. MDBList API — **WINNER (primary)**
- **What it is:** community rating aggregator (mdblist.com), Fandango/Trakt-integrated, the de-facto RT-for-TV source the whole Plex/Jellyfin/Kodi ecosystem uses. Combines IMDb, TMDb, Trakt, Letterboxd, **Rotten Tomatoes (critic + audience)**, Metacritic, RogerEbert, MyAnimeList, etc.
- **Coverage:** movies (`movie`) AND TV shows (`show`) — explicitly confirmed by the Jellyfin plugin ("Supports movies (movie) and TV shows (show)") and the TMDb-helper issue where MDBList was adopted *specifically because OMDb doesn't do Metacritic/RT for TV*. Sources: https://github.com/Druidblack/Jellyfin.Plugin.MDBList_Ratings , https://github.com/jurialmunkey/plugin.video.themoviedb.helper/issues/963
- **Endpoint (media-info / single title):**
  - Native API: `https://api.mdblist.com/` — docs OpenAPI at `https://api.mdblist.com/docs/`, Apiary at `https://mdblist.docs.apiary.io/`.
  - Lookup by IMDb ID or TMDB ID, media type `movie`|`show`. CLI form: `get media-info imdb show tt32159809`. HTTP form (per the ecosystem clients + docs): `GET https://api.mdblist.com/{provider}/{mediatype}/{id}?apikey=KEY` where provider = `imdb`/`tmdb`, mediatype = `movie`/`show` (verify exact path against `api.mdblist.com/docs/` at integration time — the Apiary/native docs are the SSoT; a newer `/title/{id}/ratings?apikey=` shape also exists on the reeldb-compatible surface).
  - Auth: append `?apikey=YOUR_KEY` (free, generated at https://mdblist.com/preferences/). OAuth2 also supported (30-day tokens) but unnecessary for a personal server script.
- **Response shape (verified via `mdblist-cli` output, `luckylittle/mdblist-cli`):**
  ```yaml
  title: The Paper
  year: 2025
  type: show            # or movie
  score: 0
  scoreaverage: 72
  ids: { imdb: tt32159809, trakt: 239158, tmdb: 253941, tvdb: 449872 }
  ratings:
    - source: imdb       value: 6.9   score: 69   votes: 981   url: ...
    - source: tomatoes    value: 93    score: 93   url: https://www.rottentomatoes.com/...   # RT CRITIC
    - source: popcorn      value: 96    score: 96   url: https://www.rottentomatoes.com/...   # RT AUDIENCE
    - source: metacritic  value: 90    score: 90   url: ...
    ...
  ```
  - **`source: tomatoes`** = RT critic Tomatometer % (this is the core need, movies+TV).
  - **`source: popcorn`** = RT audience Popcornmeter % (the nice-to-have).
  - Each rating carries a **`url`** = the canonical RT deep link (solves "read more →" — no slug guessing).
  - Rotten Tomatoes `certified_fresh` flag is exposed (Jellyfin plugin renders a Certified-Fresh badge from it).
  - Mapping confirmed by Kometa (`rating1_image: rt_tomato` ← critic, `rating2_image: rt_popcorn` ← audience) and the Jellyfin plugin badges.
- **NO consensus prose.** Explicitly confirmed by the TMDb-helper maintainer: *"Omdb has some things that mdblist doesn't like critics consensus and awards."* (https://github.com/jurialmunkey/plugin.video.themoviedb.helper/issues/963)
- **Rate limit (verified):** Free = **1000 requests/day**; paid Patreon tiers 10k/25k/100k/250k. Source: https://docs.mdblist.com/docs/api . `get my-limits` returns `{api_requests:1000, api_requests_count:N}`. 1000/day is ample for spela's cache-on-lookup-then-persist pattern (each title fetched once, cached to disk).
- **Freshness caveat:** MDBList is periodically refreshed, "often days, weeks and months out of date" for some sources. For a personal media app this is irrelevant (RT scores are near-static after release week). Cache to disk with a long TTL (e.g. 30 days, or forever for old titles).
- **Bulk lookups supported** (retrieve ratings for many titles at once) — useful if spela ever pre-warms the watchlist ("To Watch" RT-scored rows).
- **Legal/ToS:** MDBList is a first-party aggregator offering its own public API + free keys; using its API as intended is legitimate (not RT scraping by you). Low legal risk.

### 2. OMDb API — **FALLBACK (movies) + the only free consensus-prose source (movies only)**
- **What it is:** long-standing free REST movie/series/episode metadata API (`omdbapi.com`). Free key = 1000 req/day; paid Patreon tiers raise it.
- **Endpoint:** `http://www.omdbapi.com/?apikey=KEY&i=<imdbID>&tomatoes=true` (by IMDb ID) or `&t=<title>`.
- **MOVIES:** `Ratings[]` contains `{"Source":"Rotten Tomatoes","Value":"98%"}` alongside IMDb + Metacritic; with `&tomatoes=true` you also get `tomatoMeter`, `tomatoRating`, `tomatoReviews`, `tomatoFresh`, `tomatoRotten`, **`tomatoConsensus`** (the one-line critics consensus), `tomatoUserMeter`, etc. This is the ONLY free structured source of the consensus prose. (Example: One Hundred and One Dalmatians `Ratings` array shows IMDb 7.2/10, Rotten Tomatoes 98%, Metacritic 83/100.)
- **TV SERIES — NO RT DATA (verified live 2026-07-13):** requested Game of Thrones (`tt0944947`) with `&tomatoes=true`:
  - `Ratings[]` = **only** `{"Internet Movie Database","9.2/10"}` — NO Rotten Tomatoes entry.
  - `tomatoMeter: "N/A"`, `tomatoConsensus: "N/A"`, all tomato fields N/A.
  - Confirms the long-standing gap (GitHub issue #147, "Rotten Tomatoes Data for TV Shows?", never resolved) is STILL present in 2026. OMDb returns RT (and Metacritic) for movies only.
- **Returns:** IMDb id + IMDb rating + Metascore always; RT + consensus for movies. Good complement — spela can pull `tomatoConsensus` (movie), IMDb, Metascore in the same call it uses for anything else.
- **Legal/ToS:** OMDb is a legitimate free API; RT values are re-served by OMDb (not scraped by you). Low risk.

### 3. Official RT / Fandango partner API — **DEAD for individuals**
- `developer.fandango.com/rotten_tomatoes` — access is **partner/enterprise gated**: submit a Business Proposal Form, reviewed case-by-case; Fandango "no longer supports unauthorized use of their data." Not self-serve.
- Reported pricing is **enterprise-tier** (an unofficial Reddit-sourced figure cites annual fees starting ~$60k; treat as estimate, but the direction is clear — not for hobbyists). No high-res images. The old `developer.rottentomatoes.com` self-serve portal is long deprecated.
- **Verdict: not viable.** Sources: https://developer.fandango.com/rotten_tomatoes , https://www.rottentomatoes.com/help_desk

### 4. RapidAPI "Rotten Tomatoes" wrappers — **fragile paid scrapers, ToS risk**
- The recurring 2026 listing is **`matepapava123/rottentomato`** (used by a late-2025 Kaggle dataset for critic+audience scores). Others are collected at https://rapidapi.com/collection/rotten-tomatoes-api .
- These are almost universally **scrapers behind a paywall** — they parse RT pages and re-sell the result. Breakage risk is high (they break whenever RT redesigns), maintenance is out of your control, and free tiers are stingy/rate-limited.
- **Legal:** re-serving scraped RT data violates Fandango ToS; using such a wrapper inherits that exposure (mitigated somewhat by it being the vendor scraping, not you, but the data provenance is tainted).
- **Verdict: avoid** — worse on every "robust" axis (breakage, cost, ToS) than MDBList.

### 5. Watchmode API — **wrong tool (streaming availability, not scores)**
- Watchmode is built for "where can I stream this" (deep links, per-region availability), NOT aggregated critic scores. It does not cleanly provide RT critic scores. Skip for this use case. Source: https://apidog.com/blog/free-movie-apis/

### 6. Trakt API — **own ratings only, NO RT**
- Trakt's API exposes **Trakt's own** ratings/user ratings only. External ratings (IMDb/TMDb/RT/Metacritic) show on the Trakt website/apps but are **NOT exposed via the API** (open feature request as of Oct 2024, still unshipped). Tools that want RT via a Trakt-adjacent flow route through OMDb/MDBList anyway. Source: https://forums.trakt.tv/t/any-way-to-get-external-ratings-from-api/27985
- **Verdict: not an RT source.**

### 7. TMDB — **confirmed NO RT**
- TMDB carries only its own `vote_average` (audience-style). No RT. Its reviews endpoint returns user reviews, not aggregated critic scores or an RT-style consensus — not useful as a "critics summary." spela already uses TMDB for search/metadata; keep it for that, not scores.

### 8. JustWatch (unofficial API) — **no RT**
- JustWatch surfaces streaming availability + its own/IMDb/TMDB signals; not a reliable RT critic-score source. Same class as Watchmode. Skip.

### 9. Metacritic / IMDb (bundled via OMDb) — **the pragmatic alt when RT is missing**
- OMDb always returns **IMDb rating** + **Metascore** (Metacritic) for movies, and IMDb for TV. But **Metacritic via OMDb is ALSO movie-only** (same gap as RT — see TMDb-helper issue #963, which is precisely about adding Metacritic-for-TV and had to switch to MDBList to get it).
- **MDBList carries Metacritic for TV too** (normalized). So if RT-for-TV is ever thin in MDBList, Metacritic-for-TV from the same MDBList call is the natural secondary critic signal for series.

### 10. Direct RT page scraping — **the consensus-prose escape hatch; ToS-violating, higher maintenance**
- **Current 2026 page structure (verified via two independently-maintained 2026 scrapers + a browse.sh skill):** an RT movie/TV detail page carries **THREE embedded JSON blobs in the initial server-rendered HTML (no JS rendering required):**
  1. the **media-scorecard** blob (Tomatometer score/state/counts, Popcornmeter score/state/counts, `certified_fresh`),
  2. a **JSON-LD** `<script type="application/ld+json">` block with `Movie`/`TVSeries` schema + `aggregateRating`,
  3. the where-to-watch affiliate list.
  - **Critics consensus prose** is in the DOM at `id="critics-consensus" class="consensus"` (single element) AND inside the media-scorecard JSON (`critics_consensus` field). Verified shape (The Matrix): `"critics_consensus": "Thanks to the Wachowskis' imaginative vision, The Matrix is a smartly crafted combination..."`, `tomatometer.score:83`, `popcornmeter.score:85`, `certified:true`.
  - Sources: https://apify.com/jungle_synthesizer/rottentomatoes-tomatometer-scores-scraper (2026-05-28, explicitly: "Data comes from three embedded JSON blobs per page — the media scorecard, the JSON-LD schema block, and the where-to-watch affiliate list — so it's stable and doesn't depend on CSS selectors that change with redesigns") ; https://apify.com/lulzasaur/rottentomatoes-scraper (2026-04-27, "Extracts data from JSON-LD structured data and inline score JSON embedded in each page — no JavaScript rendering needed") ; https://browse.sh/skills/rottentomatoes.com/get-rating-tr6mq7.md
  - **Bot protection:** simple server-side GETs generally work against detail pages (the data is in SSR HTML); scrapers note "residential proxies recommended to avoid rate limiting" for high-volume runs, i.e. at spela's low personal volume + disk cache, occasional single GETs are typically fine but NOT guaranteed (RT can add Cloudflare challenges at any time).
- **Decay horizon:** the JSON-LD `aggregateRating` + media-scorecard blob have been stable across the last redesign and are the least-fragile parse targets (much more robust than CSS/`rt-text[slot=...]` selectors). Still, this is RT's page — it can change without notice; treat as a **best-effort, self-healing-with-fallback** path, never the primary.
- **Legal/ToS:** Fandango ToS **explicitly prohibits** all automated collection/scraping "including robots, scripts, spiders, data extractors" without express written authorization; personal-and-non-commercial-use-only clause. Scraping RT is a clear ToS violation (enforceability varies by jurisdiction; for a private self-hosted personal app the practical risk is low but the ToS breach is unambiguous). Source: https://www.rottentomatoes.com/policies/terms-of-use

---

## The "read more →" RT deep-link recommendation

**Use MDBList's per-source `url` field. It is the canonical RT URL — no guessing.** This is the robust answer and it comes free in the same response that carries the score.

If for some reason you must construct the link yourself:
- Slugs are **prefix-deterministic** (`/m/<slug>` movies, `/tv/<slug>` TV) but the **slug body is NOT reliably deterministic**: many titles = lowercase-underscored title (Inception → `/m/inception`, Breaking Bad → `/tv/breaking_bad`), BUT disambiguated titles append year/qualifier suffixes in non-guessable ways (e.g. War Dogs (2016) → `war_dogs_2016_film`, not `war_dogs_2016`). Naive slug construction WILL 404 on remakes/duplicates.
- **Robust self-construction pattern:** try the deterministic `/m/{slug}` or `/tv/{slug}` first; on 404 or ambiguity, fall back to the RT search results page and follow the canonical `url` from the first matching result.
- **But this is moot** — MDBList already hands you the correct `url`, so cache that and skip slug logic entirely. Fall back to an RT search-URL only if MDBList somehow lacks the `url` for a title. Sources: https://en.wikipedia.org/wiki/Template:Rotten_Tomatoes , https://github.com/yt-dlp/yt-dlp/issues/6729

---

## Recommended spela integration

**Arsenal (deterministic tools, cache-to-disk):**
1. **MDBList** as the primary RT source. On a title lookup (spela already has IMDb id from Torrentio + TMDB id from search), call `GET api.mdblist.com/{imdb|tmdb}/{movie|show}/{id}?apikey=KEY`, parse `ratings[]` for `source==tomatoes` (critic %) + `source==popcorn` (audience %) + `certified_fresh` + the `tomatoes` entry's `url` (the "read more →" link). Cache the whole ratings blob to disk under `~/.config/spela/` keyed by IMDb id (long TTL). 1000/day is plenty; each title fetched once.
2. **OMDb** as a supplement: same lookup pulls `tomatoConsensus` **for movies** (consensus prose), plus IMDb rating + Metascore. TV consensus stays empty — that's expected; the UI should degrade gracefully (show score, omit consensus for series).
3. **Metacritic-from-MDBList** as the secondary critic signal for TV (since MDBList has Metacritic-for-TV where OMDb does not) — optional.

**Consensus-prose policy (the honest gap):** movies get consensus via OMDb `tomatoConsensus`; **TV series get score-only, no consensus** from any clean free source. Do NOT block the watch-tracker/recommender feature on TV consensus. If TV consensus prose ever becomes a hard requirement, the only paths are RT page scraping (`critics_consensus` in the media-scorecard blob, ToS-violating, maintenance risk) or a paid scraper wrapper — both are last resorts, not the low-maintenance answer spela wants.

**Cache-key + refresh:** key by IMDb id (spela's existing SSoT); refresh TTL ~30d for recent titles, effectively-permanent for older ones (RT scores stabilize). This keeps daily API calls near-zero after warm-up and makes both APIs' free tiers a non-issue.

**Legal posture:** using MDBList's and OMDb's own APIs (both offer free keys and intend this use) is clean. Avoid direct RT scraping and RapidAPI scraper wrappers as the standing mechanism (Fandango ToS prohibits automated RT collection); keep scraping only as a manual/emergency fallback if ever needed, at personal volume.

---

## Sources (all accessed 2026-07-13)
- MDBList API docs + rate limits: https://docs.mdblist.com/docs/api , https://docs.mdblist.com/
- MDBList native API + response shape (CLI): https://github.com/luckylittle/mdblist-cli , https://pkg.go.dev/github.com/luckylittle/mdblist-cli , https://mdblist.docs.apiary.io/
- MDBList RT coverage for TV (Jellyfin plugin): https://github.com/Druidblack/Jellyfin.Plugin.MDBList_Ratings , https://github.com/Druidblack/jellyfin_ratings
- MDBList adopted for TV critic scores where OMDb fails + "mdblist doesn't do consensus/awards": https://github.com/jurialmunkey/plugin.video.themoviedb.helper/issues/963
- OMDb RT-for-TV gap (issue): https://github.com/omdbapi/OMDb-API/issues/147 ; live verification GoT tt0944947 (RT/consensus all N/A) via omdbapi.com
- OMDb movie Ratings array (RT + Metacritic): https://stackoverflow.com/questions/70500432/
- Official RT/Fandango partner API (gated): https://developer.fandango.com/rotten_tomatoes , https://www.rottentomatoes.com/help_desk
- RapidAPI RT wrappers: https://rapidapi.com/collection/rotten-tomatoes-api , https://rapidapi.com/matepapava123/api/rottentomato
- Watchmode / free movie APIs: https://apidog.com/blog/free-movie-apis/
- Trakt API has no external ratings: https://forums.trakt.tv/t/any-way-to-get-external-ratings-from-api/27985
- RT page structure 2026 (3 JSON blobs, JSON-LD, no-JS, consensus fields): https://apify.com/jungle_synthesizer/rottentomatoes-tomatometer-scores-scraper , https://apify.com/lulzasaur/rottentomatoes-scraper , https://browse.sh/skills/rottentomatoes.com/get-rating-tr6mq7.md
- RT slug determinism + search fallback: https://en.wikipedia.org/wiki/Template:Rotten_Tomatoes , https://github.com/yt-dlp/yt-dlp/issues/6729
- Fandango/RT ToS (scraping prohibited): https://www.rottentomatoes.com/policies/terms-of-use
