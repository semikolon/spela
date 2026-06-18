use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Torrentio API — aggregates 24 torrent sites (TPB, 1337x, YTS, RARBG, TorrentGalaxy, etc.)
/// Default providers already return 76+ results per movie. All-providers URL doesn't add more.
/// Other Stremio addons (MediaFusion, Knightcrawler, Comet) require encrypted config URLs.
const TORRENTIO_BASE: &str = "https://torrentio.strem.fun/sort=seeders";

const PUBLIC_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.bittor.pw:1337/announce",
    "udp://explodie.org:6969/announce",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show: Option<ShowInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searching: Option<EpisodeRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub torrent_available: bool,
    pub results: Vec<TorrentResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowInfo {
    pub tmdb_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seasons: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_episode: Option<EpisodeRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_episode: Option<EpisodeRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    /// Apr 28, 2026 (Apr 29 corrected): Full TMDB poster URL, already
    /// prefixed with the image base (`https://image.tmdb.org/t/p/w500{poster_path}`).
    /// Populated at search time, persisted into `last_search.json`, plumbed
    /// through `PlayRequest.poster_url` → `CurrentStream.poster_url` →
    /// `CastMetadata` so the Default Media Receiver shows a poster + title
    /// splash on top of the playback view.  Does NOT govern overlay-mode
    /// (that's stream-type-dependent — see spela CLAUDE.md § "DMR overlay
    /// is stream-type-dependent").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeRef {
    pub season: u32,
    pub episode: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TvEpisodeIntent {
    None,
    FirstEpisode,
    LatestEpisode,
    LatestSeasonFirstEpisode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentResult {
    pub id: usize,
    pub quality: String,
    pub title: String,
    pub seeds: u32,
    pub size: String,
    pub source: String,
    pub magnet: String,
    pub info_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_index: Option<u32>,
}

pub struct SearchEngine {
    client: reqwest::Client,
    tmdb_key: String,
}

impl SearchEngine {
    pub fn new(tmdb_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            tmdb_key,
        }
    }

    /// Web-remote T-4: best-effort TMDB poster for a library entry by
    /// parsed title (+ parsed year when known). Fully fail-soft — empty
    /// key / blank title / no hit / network error all yield `None`, and
    /// the SPA renders a clean titled fallback tile (AC-3.2). Snappy:
    /// returns at the FIRST hit; the extra attempts fire ONLY on a miss.
    /// Research-tuned 2026-05-18: lifts the title-only ~61% by
    /// disambiguating remakes/typos/foreign + catching TV-classified
    /// entries, all on the existing TMDB key, no new dep. Attempt order
    /// (a) `/search/movie` + `&year=` when year known (best precision),
    /// then (b) `/search/movie` no year (release-name years are often
    /// off-by-one vs TMDB `primary_release_year`), then (c)
    /// `/search/multi` (catches TMDB-as-TV / alt media; person results
    /// carry no `poster_path` so are skipped). First poster-bearing
    /// result wins; no movie-details round-trip (search hits already
    /// carry `poster_path`); does NOT touch the stabilised `tmdb_search`
    /// used by the main search/ranker path.
    pub async fn movie_poster(&self, title: &str, year: Option<u32>) -> Option<String> {
        if self.tmdb_key.is_empty() || title.trim().is_empty() {
            return None;
        }
        // Strip scene-release noise (SxxExx, resolution/source/codec/tags,
        // release-group dashes) so the TMDB query is the bare work title —
        // the dominant cause of misses (esp. TV: "Black Mirror S01 720p
        // HDTV x264" → "Black Mirror"). Boundary-only: LibraryEntry.title
        // is untouched, so play/bypass matching is unaffected.
        let cleaned = clean_title_for_tmdb(title);
        let title = if cleaned.is_empty() {
            title
        } else {
            cleaned.as_str()
        };
        let q = urlencoded(title);
        let mut urls: Vec<String> = Vec::with_capacity(3);
        if let Some(y) = year {
            urls.push(format!(
                "https://api.themoviedb.org/3/search/movie?query={}&year={}&api_key={}",
                q, y, self.tmdb_key
            ));
        }
        urls.push(format!(
            "https://api.themoviedb.org/3/search/movie?query={}&api_key={}",
            q, self.tmdb_key
        ));
        urls.push(format!(
            "https://api.themoviedb.org/3/search/multi?query={}&api_key={}",
            q, self.tmdb_key
        ));
        // Relaxed last-resort tiers — TMDB does NOT Levenshtein-correct
        // internal-char typos ("American Psyco" → 0 results for every
        // form above), so broaden the QUERY (typo is usually not in the
        // first word) and let the similarity scorer pick the right film
        // from the wider candidate set. Only reached on a miss (no STRONG
        // early-out), so the 51 happy-path stays 1 call / snappy.
        let words: Vec<&str> = title.split_whitespace().collect();
        if words.len() >= 2 {
            let head = urlencoded(&words[..words.len() - 1].join(" ")); // drop last word
            let first = urlencoded(words[0]); // first word only
            if let Some(y) = year {
                urls.push(format!(
                    "https://api.themoviedb.org/3/search/movie?query={}&year={}&api_key={}",
                    head, y, self.tmdb_key
                ));
                urls.push(format!(
                    "https://api.themoviedb.org/3/search/movie?query={}&year={}&api_key={}",
                    first, y, self.tmdb_key
                ));
            }
            urls.push(format!(
                "https://api.themoviedb.org/3/search/multi?query={}&api_key={}",
                head, self.tmdb_key
            ));
        }
        // Score candidates by token containment (fraction of query words
        // that fuzzy-appear in the candidate title) instead of blindly
        // taking the first poster-bearing hit. Token-level — NOT
        // whole-string Levenshtein, which over-penalises legitimate
        // length differences (release-cleaned "Lord Of The Rings
        // Fellowship" vs TMDB "The Lord of the Rings: The Fellowship of
        // the Ring" is a CORRECT subset, not a mismatch). Per-word fuzzy
        // matching still recovers typos ("psyco"~"psycho"). This both
        // lifts recall AND fixes a latent bug — a typo'd query that
        // fuzzy-returned a *wrong* film with a poster used to yield that
        // wrong poster; below ACCEPT we now return None (titled fallback,
        // AC-3.2, is strictly better than a confidently-wrong poster).
        // Snappy: a full-containment hit (== STRONG) early-returns and
        // skips the remaining attempts, so the common case is 1 call.
        // ACCEPT is intentionally a hair above zero: take the BEST-overlap
        // poster across all attempts, rejecting only true zero-overlap
        // garbage. This preserves blind-first recall (best-of >= first-of)
        // while adding typo recovery + better selection — per the user's
        // stated priority (more posters), not a stricter rejection.
        const STRONG: f32 = 1.0;
        const ACCEPT: f32 = 0.01;
        let want = title_norm(title);
        let mut best: Option<(f32, String)> = None;
        for url in urls {
            let Ok(resp) = self.client.get(&url).send().await else {
                continue;
            };
            let Ok(v) = resp.json::<Value>().await else {
                continue;
            };
            let Some(arr) = v["results"].as_array() else {
                continue;
            };
            for item in arr {
                let Some(poster) = tmdb_poster_url(item["poster_path"].as_str()) else {
                    continue;
                };
                let cand = item["title"]
                    .as_str()
                    .or_else(|| item["name"].as_str())
                    .or_else(|| item["original_title"].as_str())
                    .unwrap_or("");
                let s = title_token_score(&want, &title_norm(cand));
                if s >= STRONG {
                    return Some(poster);
                }
                if best.as_ref().is_none_or(|(bs, _)| s > *bs) {
                    best = Some((s, poster));
                }
            }
        }
        best.filter(|(s, _)| *s >= ACCEPT).map(|(_, p)| p)
    }

    pub async fn search(
        &self,
        query: &str,
        movie: bool,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<SearchResult> {
        if self.tmdb_key.is_empty() {
            return Ok(SearchResult {
                query: query.into(),
                show: None,
                searching: None,
                error: Some("TMDB_API_KEY not set. Get one free at themoviedb.org".into()),
                torrent_available: false,
                results: vec![],
            });
        }

        // Parse inline season/episode markers like "S05E06", "5x06", or
        // "season 5 episode 6" out of the query string. Explicit
        // --season/--episode CLI flags WIN over parsed markers.
        //
        // May 6, 2026 incident (the "endlessly proud about streaming"
        // loop): Ruby's Gemini tool-loop issued
        // `spela search "The Boys S05E06"` (no flags); TMDB choked on
        // the marker; search returned empty (52 chars); Ruby narrated
        // success anyway because she'd sometimes hit the working
        // `--season N --episode M` form on a retry. Result: 6
        // contradictory "now streaming" / "hasn't surfaced" claims in
        // 97 seconds. Pre-parsing the marker on the engine side makes
        // the natural-language form work first time, every time.
        let (cleaned_query, parsed_season, parsed_episode) = parse_episode_markers(query);
        let (cleaned_query, tv_episode_intent) = if movie {
            (cleaned_query, TvEpisodeIntent::None)
        } else {
            parse_tv_episode_intent(&cleaned_query)
        };
        let q_owned: String;
        let q: &str = if parsed_season.is_some()
            || parsed_episode.is_some()
            || tv_episode_intent != TvEpisodeIntent::None
        {
            q_owned = cleaned_query;
            &q_owned
        } else {
            query
        };
        let final_season = season.or(parsed_season);
        let final_episode = episode.or(parsed_episode);

        if movie {
            self.search_movie(q).await
        } else if final_season.is_some() || final_episode.is_some() {
            // Explicit season/episode = definitely TV
            self.search_tv(q, final_season, final_episode, tv_episode_intent)
                .await
        } else {
            // Auto-detect: use TMDB multi-search to determine if it's a movie or TV show
            match self.tmdb_auto_detect(q).await {
                Ok(media_type) if media_type == "movie" => {
                    tracing::info!("Auto-detected '{}' as movie (TMDB multi-search)", q);
                    self.search_movie(q).await
                }
                _ => {
                    // Default to TV, or if auto-detect found "tv"
                    self.search_tv(q, final_season, final_episode, tv_episode_intent)
                        .await
                }
            }
        }
    }

    /// Use TMDB's /search/multi endpoint to detect whether a query is a movie or TV show.
    ///
    /// 2026-06-09 fix (TODO entry R, `dotfiles/TODO.md`): previously picked
    /// the FIRST non-person result, which fails on short queries — TMDB's
    /// popularity ordering surfaced unrelated noise (canonical bug:
    /// `Spring` → `Send Help (2024)` instead of `Spring (2014)`). Now scores
    /// every candidate by token-similarity + year + vote_count, and returns
    /// the media_type of the BEST candidate. If no candidate clears the
    /// confidence floor, falls back to the first non-person result (old
    /// behavior) so we never regress on genuine zero-overlap cases.
    async fn tmdb_auto_detect(&self, query: &str) -> Result<String> {
        let (q_clean, q_year) = extract_year_from_query(query);
        let api_q = if q_clean.is_empty() { query } else { &q_clean };
        let url = format!(
            "https://api.themoviedb.org/3/search/multi?query={}&api_key={}",
            urlencoded(api_q),
            self.tmdb_key
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;
        let results = resp["results"]
            .as_array()
            .ok_or_else(|| anyhow!("No multi-search results"))?;

        // Filter to movie/tv (skip "person"), then score-then-pick.
        let candidates: Vec<&Value> = results
            .iter()
            .filter(|r| matches!(r["media_type"].as_str(), Some("movie") | Some("tv")))
            .collect();
        if let Some(best) = pick_best_tmdb_candidate(&candidates, api_q, q_year) {
            let mt = best["media_type"]
                .as_str()
                .ok_or_else(|| anyhow!("Best multi-search candidate missing media_type"))?;
            return Ok(mt.to_string());
        }
        // Fallback: confident-pick failed; preserve old "first non-person" behavior
        // so genuinely ambiguous queries still return SOMETHING.
        if let Some(first) = candidates.first() {
            if let Some(mt) = first["media_type"].as_str() {
                return Ok(mt.to_string());
            }
        }
        Err(anyhow!("No movie or TV result found"))
    }

    async fn search_tv(
        &self,
        query: &str,
        season: Option<u32>,
        episode: Option<u32>,
        intent: TvEpisodeIntent,
    ) -> Result<SearchResult> {
        // Step 1: TMDB search
        let tmdb = self.tmdb_search(query, "tv").await?;
        let tmdb_id = tmdb["id"]
            .as_u64()
            .ok_or_else(|| anyhow!("No TV show found for \"{}\"", query))?;

        // Step 2: Get details + IMDB ID
        let detail = self.tmdb_tv_details(tmdb_id).await?;
        let imdb_id = detail["external_ids"]["imdb_id"].as_str().map(String::from);

        let show_info = ShowInfo {
            tmdb_id,
            imdb_id: imdb_id.clone(),
            title: detail["name"].as_str().unwrap_or("Unknown").into(),
            seasons: detail["number_of_seasons"].as_u64().map(|n| n as u32),
            status: detail["status"].as_str().map(String::from),
            latest_episode: extract_episode(&detail["last_episode_to_air"]),
            next_episode: extract_episode(&detail["next_episode_to_air"]),
            release_date: None,
            overview: None,
            poster_url: tmdb_poster_url(detail["poster_path"].as_str()),
        };

        let imdb_id = match &show_info.imdb_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => {
                return Ok(SearchResult {
                    query: query.into(),
                    show: Some(show_info),
                    searching: None,
                    error: Some("No IMDB ID found for this show".into()),
                    torrent_available: false,
                    results: vec![],
                })
            }
        };

        let (s, e) = episode_to_search(season, episode, intent, show_info.latest_episode.as_ref());

        // Step 3: Torrentio lookup (filtered by show title to drop spurious
        // cross-show results — the Apr 15 "French Chef for The Boys S05E03"
        // incident).
        let results = self
            .torrentio_streams(&imdb_id, &show_info.title, Some(s), Some(e))
            .await?;
        Ok(SearchResult {
            query: query.into(),
            show: Some(show_info),
            searching: Some(EpisodeRef {
                season: s,
                episode: e,
                name: None,
                air_date: None,
            }),
            error: None,
            torrent_available: !results.is_empty(),
            results,
        })
    }

    async fn search_movie(&self, query: &str) -> Result<SearchResult> {
        let tmdb = self.tmdb_search(query, "movie").await?;
        let tmdb_id = tmdb["id"]
            .as_u64()
            .ok_or_else(|| anyhow!("No movie found for \"{}\"", query))?;

        let detail = self.tmdb_movie_details(tmdb_id).await?;
        let imdb_id = detail["external_ids"]["imdb_id"]
            .as_str()
            .or_else(|| detail["imdb_id"].as_str())
            .map(String::from);

        let show_info = ShowInfo {
            tmdb_id,
            imdb_id: imdb_id.clone(),
            title: detail["title"].as_str().unwrap_or("Unknown").into(),
            seasons: None,
            status: None,
            latest_episode: None,
            next_episode: None,
            release_date: detail["release_date"].as_str().map(String::from),
            overview: detail["overview"]
                .as_str()
                .map(|s| s.chars().take(200).collect()),
            poster_url: tmdb_poster_url(detail["poster_path"].as_str()),
        };

        let imdb_id = match &show_info.imdb_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => {
                return Ok(SearchResult {
                    query: query.into(),
                    show: Some(show_info),
                    searching: None,
                    error: Some("No IMDB ID".into()),
                    torrent_available: false,
                    results: vec![],
                })
            }
        };

        let results = self
            .torrentio_streams(&imdb_id, &show_info.title, None, None)
            .await?;
        Ok(SearchResult {
            query: query.into(),
            show: Some(show_info),
            searching: None,
            error: None,
            torrent_available: !results.is_empty(),
            results,
        })
    }

    /// Typed TMDB search (`/search/movie` or `/search/tv`).
    ///
    /// 2026-06-09 fix (TODO entry R): previously picked `results.first()`,
    /// which is wrong for short queries because TMDB orders by popularity
    /// regardless of title match. Now strips year from query (passes via
    /// `&year=` / `&first_air_date_year=` for precision), then scores every
    /// candidate by token-similarity + year + vote_count and picks the
    /// best. Falls back to first result if no candidate clears the floor,
    /// preserving old behavior for genuine zero-overlap cases.
    async fn tmdb_search(&self, query: &str, media_type: &str) -> Result<Value> {
        let (q_clean, q_year) = extract_year_from_query(query);
        let api_q = if q_clean.is_empty() { query } else { &q_clean };
        // TMDB's typed search accepts a year filter — use it when extractable.
        // `/search/movie` uses `&year=` (primary_release_year);
        // `/search/tv`    uses `&first_air_date_year=`.
        let year_param = match (q_year, media_type) {
            (Some(y), "movie") => format!("&year={}", y),
            (Some(y), "tv") => format!("&first_air_date_year={}", y),
            _ => String::new(),
        };
        let url = format!(
            "https://api.themoviedb.org/3/search/{}?query={}{}&api_key={}",
            media_type,
            urlencoded(api_q),
            year_param,
            self.tmdb_key
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;
        let results = resp["results"]
            .as_array()
            .ok_or_else(|| anyhow!("No {} found for \"{}\"", media_type, query))?;
        let refs: Vec<&Value> = results.iter().collect();
        if let Some(best) = pick_best_tmdb_candidate(&refs, api_q, q_year) {
            return Ok(best.clone());
        }
        // Fallback preserves old behavior for queries where nothing
        // confidently matches (e.g. typos beyond title_similarity's reach).
        refs.first()
            .map(|v| (*v).clone())
            .ok_or_else(|| anyhow!("No {} found for \"{}\"", media_type, query))
    }

    async fn tmdb_tv_details(&self, tmdb_id: u64) -> Result<Value> {
        let url = format!(
            "https://api.themoviedb.org/3/tv/{}?api_key={}&append_to_response=external_ids",
            tmdb_id, self.tmdb_key
        );
        Ok(self.client.get(&url).send().await?.json().await?)
    }

    async fn tmdb_movie_details(&self, tmdb_id: u64) -> Result<Value> {
        let url = format!(
            "https://api.themoviedb.org/3/movie/{}?api_key={}&append_to_response=external_ids",
            tmdb_id, self.tmdb_key
        );
        Ok(self.client.get(&url).send().await?.json().await?)
    }

    async fn torrentio_streams(
        &self,
        imdb_id: &str,
        show_title: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<Vec<TorrentResult>> {
        let path = match (season, episode) {
            (Some(s), Some(e)) => format!("stream/series/{}:{}:{}.json", imdb_id, s, e),
            _ => format!("stream/movie/{}.json", imdb_id),
        };
        let url = format!("{}/{}", TORRENTIO_BASE, path);

        let resp: Value = self
            .client
            .get(&url)
            .header("User-Agent", "spela/2.0")
            .send()
            .await?
            .json()
            .await?;

        let streams = resp["streams"].as_array().cloned().unwrap_or_default();
        let results: Vec<TorrentResult> = streams
            .iter()
            .map(|s| {
                let title_text = s["title"].as_str().unwrap_or("");
                let meta = parse_torrentio_title(title_text);
                let quality = s["name"]
                    .as_str()
                    .unwrap_or("")
                    .replace("Torrentio\n", "")
                    .trim()
                    .to_string();
                let info_hash = s["infoHash"].as_str().unwrap_or("").to_string();
                let filename = s["behaviorHints"]["filename"]
                    .as_str()
                    .or_else(|| title_text.split('\n').next())
                    .unwrap_or("Unknown")
                    .to_string();

                TorrentResult {
                    id: 0, // assigned after sorting
                    quality,
                    title: filename,
                    seeds: meta.0,
                    size: meta.1,
                    source: meta.2,
                    magnet: build_magnet(
                        &info_hash,
                        s["behaviorHints"]["filename"].as_str().unwrap_or(""),
                    ),
                    info_hash,
                    file_index: s["fileIdx"].as_u64().map(|n| n as u32),
                }
            })
            .collect();

        // Filter spurious cross-show results BEFORE ranking, so result IDs
        // assigned by rank_results_mut reflect only legitimate matches.
        // Apr 15 incident: Torrentio returned "The.French.Chef.Season.05.03of20"
        // as a top-seeded candidate for a `The Boys` S05E03 query (IMDb-ID
        // routed, no cross-show data should have been possible — but it was).
        let mut results = filter_results_by_show_title(results, show_title);
        rank_results_mut(&mut results);
        Ok(results.into_iter().take(8).collect())
    }
}

/// Detect HEVC/x265 from torrent filename — these need NVENC re-encoding
/// for Chromecast.
///
/// Apr 15, 2026 fix: some release groups (e.g. `playWEB`) format the codec
/// as `H 265` with a literal space instead of `h265` / `h.265`. The Apr 15
/// S05E02 incident: `The Boys S05E02 Teenage Kix 2160p AMZN WEB-DL DDP5 1
/// Atmos H 265-playWEB.mkv` was ranked as result 1 above a 1080p H.264
/// release because `is_hevc_from_title` didn't match "h 265" and treated it
/// as H.264. Ruby then played the 4K HEVC → NVENC couldn't real-time
/// transcode → bad. We now normalize runs of whitespace/dots between the
/// codec family letter and the version number before checking.
/// Canonical torrent-result ranker, shared between production and tests.
/// Sorts `results` in place using the 5-tier order, then assigns 1-based ids.
///
/// **Apr 15 v3.1.0 policy rework**: 1080p is the Sarpetorp sweet spot (TVs
/// max at 1080p native; 4K is wasted) AND HEVC→H.264 transcode at 1080p is
/// fast enough on Darwin's GTX 1650 (~6x realtime) to absorb the latency
/// cost. The old v3.0.0 policy put H.264 insta-play above resolution
/// (tier 2 dominated tier 4); the new policy inverts this because a 1080p
/// HEVC transcode is preferable to a 720p H.264 insta-play on a 1080p TV.
/// 2160p is demoted BELOW 480p because (a) no display benefit, (b) 4K
/// NVENC transcode is ~3x heavier than 1080p, (c) it's a bandwidth sink.
///
/// **Tiers (first disagreement wins)**:
/// 1. **Single-file > season pack.** `webtorrent -s` is unreliable for
///    multi-file torrents — most single-file torrents cast fastest and
///    most reliably on Chromecast.
/// 2. **Non-Dolby-Vision > Dolby Vision.** HARD GPU gate, not a
///    preference: NVENC on GTX 1650 cannot decode DV profile 5/7 RPU
///    NAL units cleanly. It logs "RPU validation failed" every frame and
///    transcodes at 0.937x realtime — unviable for live streaming. DV
///    is demoted before anything else so a non-viable DV release can
///    never win a lower tier. Promoted from v3.0.0 tier 3 because it's
///    a hardware limit, not a quality judgment.
/// 3. **Target resolution preference**: `1080p > 720p > 480p > 2160p >
///    unknown`, only when the higher-preference option has ≥50 seeds.
///    1080p is the native resolution of every TV in the house (Apr 2026);
///    720p and 480p are reasonable fallbacks if 1080p isn't viable;
///    2160p is dead-last because its pixels are discarded by the TV
///    scaler AND the extra transcode cost is meaningful on a GTX 1650.
///    Raised above codec preference in v3.1.0 because HEVC 1080p → H.264
///    1080p transcode is fast enough that the quality win beats the
///    insta-play loss vs 720p H.264.
/// 4. **H.264 > HEVC within same resolution.** Insta-play tiebreak when
///    two releases are at the same target resolution + same DV status.
///    Demoted from v3.0.0 top-level tier 2 because HEVC transcode at
///    1080p is acceptable; still matters within the same resolution
///    bucket because 1080p H.264 is strictly faster to play than 1080p
///    HEVC (no transcode).
/// 5. **More seeds > fewer seeds** — final tiebreak within the same
///    resolution + codec bucket.
/// May 13, 2026 v3.4.1 — composite resolution-with-viability tier value.
///
/// v3.4.0's tier 3 used an asymmetric seed-viability gate (≥50 seeds on the
/// HIGHER-resolution operand to fire). That produced a non-transitive
/// comparator — the May 13 PM Night Manager S02E05 fixture exposed a 3-way
/// cycle (1080p HEVC 65 > 720p H.264 51 via tier 3; 1080p H.264 17 > 1080p
/// HEVC 65 via tier 4; 720p H.264 51 > 1080p H.264 17 via tier 5 because
/// tier 3 fell through on insufficient seeds). Rust's `sort_by` requires a
/// total order; non-transitive input produces undefined output.
///
/// v3.4.1 fix: bake viability into the resolution-tier value itself, so tier 3
/// always provides a STRICT total order on `effective_res_tier`. An unviable
/// 1080p (e.g. 17 seeds) demotes to bucket 3 — below any viable 720p (bucket
/// 1) and any viable 480p (bucket 2). Tier 4 then only fires within the same
/// effective bucket (i.e. both viable 1080p, both viable 720p, etc.) where
/// the H.264-over-HEVC tiebreak is well-defined.
///
/// **Bucket mapping** (lower = ranked higher):
///   0 → 1080p with ≥50 seeds (target, fully viable)
///   1 →  720p with ≥50 seeds
///   2 →  480p with ≥50 seeds
///   3 → 1080p with <50 seeds (unviable, demoted below all viable lower tiers)
///   4 →  720p with <50 seeds
///   5 →  480p with <50 seeds
///   6 → 2160p / 4K / UHD (any seed count — Sarpetorp policy demotes 4K)
///   7 → unknown / unclassified resolution
///
/// **Generic lesson** (consider when designing other multi-criteria
/// comparators): pairwise threshold-fallthrough rules ("if operand X >
/// threshold do this, else fall to next tier") are a classic source of
/// non-transitive comparators. Bake all per-operand attributes into a SINGLE
/// per-operand value, then compare values directly — total order is then
/// structurally guaranteed.
pub(crate) fn effective_res_tier(r: &TorrentResult) -> u32 {
    const MIN_SEEDS_FOR_RESOLUTION_PREF: u32 = 50;
    let base = resolution_tier(&r.title);
    let viable = r.seeds >= MIN_SEEDS_FOR_RESOLUTION_PREF;
    match (base, viable) {
        (0, true) => 0,  // 1080p viable
        (1, true) => 1,  // 720p viable
        (2, true) => 2,  // 480p viable
        (0, false) => 3, // 1080p unviable → demoted
        (1, false) => 4, // 720p unviable
        (2, false) => 5, // 480p unviable
        (3, _) => 6,     // 2160p — always demoted per Sarpetorp policy
        _ => 7,          // unknown / unclassified
    }
}

pub fn rank_results_mut(results: &mut Vec<TorrentResult>) {
    const MIN_SEEDS_FOR_CODEC_PREF: u32 = 5;
    // May 13, 2026 v3.4.0: when the HEVC alternative has ≥SEED_DISPARITY_OVERRIDE×
    // the seeds of the H.264 tier-4 winner, override the codec preference. See
    // tier 4 body below for the full rationale + Apr/May 2026 anchoring incident.
    const SEED_DISPARITY_OVERRIDE: u32 = 30;

    results.sort_by(|a, b| {
        // Tier 1: single-file > pack
        let a_single = a.file_index.map_or(true, |i| i == 0);
        let b_single = b.file_index.map_or(true, |i| i == 0);
        if a_single != b_single {
            return if a_single {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }

        // Tier 2: non-DV > DV (HARD GPU gate, fires before any quality tier)
        let a_dv = has_dolby_vision_in_title(&a.title);
        let b_dv = has_dolby_vision_in_title(&b.title);
        if a_dv != b_dv {
            return if a_dv {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }

        // Tier 3 (v3.4.1): composite `effective_res_tier` value that bakes
        // seed-viability into the resolution bucket. See `effective_res_tier`
        // doc for the full mapping + the non-transitive-comparator history
        // that motivated the redesign. Direct `cmp` on the bucket guarantees
        // total ordering; tier 4 only fires within the same bucket.
        let a_eff = effective_res_tier(a);
        let b_eff = effective_res_tier(b);
        if a_eff != b_eff {
            return a_eff.cmp(&b_eff);
        }

        // Tier 4: H.264 > HEVC within same resolution + DV status (insta-play tiebreak).
        // May 13, 2026 v3.4.0 amendment — seed-disparity override:
        // when the HEVC alternative has ≥30× the seeds of the H.264 winner,
        // promote the HEVC. Rationale: well-seeded swarms (MeGusta-class,
        // 1000+ seeds) start streaming within seconds, while starved swarms
        // (Cinecalidad 99-seed Apr/May 2026 case) blocked librqbit's
        // first-piece fetch past ffmpeg's reconnect budget — 0 segments,
        // 75 s blue-cast icon, manual recovery. HEVC→H.264 NVENC transcode
        // on Darwin's GTX 1650 adds 5-10 s of cold-start cost, strictly
        // cheaper than waiting for a starved swarm or failing entirely. The
        // 30× threshold is "user-tuned conservative" — at 30× the H.264
        // winner is unambiguously inferior; below 30× the codec-cost
        // tradeoff isn't worth flipping. Per-resolution + DV gates still
        // fire first (tier 3 / tier 2), so this override only ever swaps
        // codec WITHIN the same resolution + DV bucket.
        let a_hevc = is_hevc_from_title(&a.title);
        let b_hevc = is_hevc_from_title(&b.title);
        if a_hevc != b_hevc {
            let (h264_seeds, hevc_seeds, h264_is_a) = if a_hevc {
                (b.seeds, a.seeds, false)
            } else {
                (a.seeds, b.seeds, true)
            };
            // `max(1)` guards h264_seeds = 0 so the multiplier stays meaningful
            // (without it, saturating_mul yields 0 and any positive HEVC count
            // trivially satisfies the inequality — semantically fine but
            // makes the threshold a no-op for that edge case).
            let h264_seeds_safe = h264_seeds.max(1);
            if hevc_seeds >= h264_seeds_safe.saturating_mul(SEED_DISPARITY_OVERRIDE) {
                return if h264_is_a {
                    std::cmp::Ordering::Greater // H.264 (a) loses to HEVC (b)
                } else {
                    std::cmp::Ordering::Less // H.264 (b) loses to HEVC (a)
                };
            }
            // No qualifying disparity — apply the existing H.264 preference
            // if the H.264 winner has viable seeds (≥5).
            let preferred = if a_hevc { b } else { a }; // the H.264 one
            if preferred.seeds >= MIN_SEEDS_FOR_CODEC_PREF {
                return if a_hevc {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
        }

        // Tier 5: more seeds > fewer seeds
        b.seeds.cmp(&a.seeds)
    });

    for (i, r) in results.iter_mut().enumerate() {
        r.id = i + 1;
    }
}

/// Drop torrent results whose title doesn't contain all significant tokens
/// from the requested show's name. Apr 15, 2026 fix for the "French Chef
/// for The Boys S05E03" incident where Torrentio's IMDb-ID-routed endpoint
/// returned a completely different show's release as a top-seeded candidate.
/// Safety net: if filtering would drop ALL results, the unfiltered list is
/// returned instead (user gets SOMETHING to play, with a warning log).
///
/// Matching is token-based with stop-word filtering:
/// - Lowercased, split on non-alphanumerics.
/// - Common English articles / prepositions (`the`, `a`, `of`, etc.) are
///   dropped — "The Boys" has only `boys` as a significant token; "Game
///   of Thrones" has `game` and `thrones`.
/// - All significant tokens must appear somewhere in the result's title
///   string (substring match on the lowercased title).
///
/// Edge cases:
/// - Empty significant-token set (show title was all stop words): no
///   filter applied, returns the full list.
/// - Filter drops everything: returns the full list with a warning,
///   because "some spurious results" is less bad than "no results at all".
fn filter_results_by_show_title(
    results: Vec<TorrentResult>,
    show_title: &str,
) -> Vec<TorrentResult> {
    let tokens = extract_significant_tokens(show_title);
    if tokens.is_empty() {
        return results;
    }
    let (matching, dropped): (Vec<_>, Vec<_>) = results.into_iter().partition(|r| {
        let lower = r.title.to_lowercase();
        tokens.iter().all(|t| lower.contains(t.as_str()))
    });
    if matching.is_empty() && !dropped.is_empty() {
        tracing::warn!(
            "Show-title filter dropped all {} Torrentio result(s) for tokens {:?} — returning unfiltered (user will see mixed results)",
            dropped.len(), tokens
        );
        return dropped;
    }
    if !dropped.is_empty() {
        tracing::info!(
            "Show-title filter dropped {} cross-show result(s) not matching {:?}",
            dropped.len(),
            tokens
        );
    }
    matching
}

/// Extract the significant (non-stop-word) tokens from a show title.
/// Used by `filter_results_by_show_title`.
///
/// Stop words are English articles + common prepositions that appear
/// in scene release naming but don't help identify a specific show:
/// `the`, `a`, `an`, `of`, `and`, `in`, `on`, `to`, `at`, `is`.
/// Everything else after lowercasing + splitting on non-alphanumeric is
/// kept as a required token.
fn extract_significant_tokens(title: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &["the", "a", "an", "of", "and", "in", "on", "to", "at", "is"];
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP_WORDS.contains(w))
        .map(String::from)
        .collect()
}

/// Classify a torrent title into a resolution bucket. Lower is better.
/// Used by tier 3 of `rank_results_mut`.
///
/// **Apr 15 v3.1.0 ordering** (Sarpetorp policy: 1080p target, 4K demoted):
///
///   0 → 1080p (target — native TV resolution + fast transcode)
///   1 → 720p  (fallback when 1080p isn't viable)
///   2 → 480p  (last viable fallback for dead 1080p/720p)
///   3 → 2160p / 4K / UHD (DEMOTED — TVs can't display the extra pixels,
///        4K NVENC transcode is ~3x heavier than 1080p, pure waste)
///   4 → unknown / anything else (sorts last)
///
/// Pattern matches are tolerant to the usual scene release separators:
/// `1080p`, `1080P`, `1080 p`, `1080.p`. 2160p also matches `4k` / `4K`
/// / `uhd` tags. Called on lowercased title internally.
///
/// If you upgrade the house to 4K TVs, flip the ordering so 2160p is 0
/// and 1080p is 1 — that's the only change needed in the ranker's
/// resolution bucket semantics.
fn resolution_tier(title: &str) -> u32 {
    let lower = title.to_lowercase();
    // Normalize separator before `p` so `1080 p` / `1080.p` count.
    let res_match = |needle: &str| -> bool {
        if lower.contains(needle) {
            return true;
        }
        let with_space = needle.replace('p', " p");
        if lower.contains(&with_space) {
            return true;
        }
        let with_dot = needle.replace('p', ".p");
        lower.contains(&with_dot)
    };
    if res_match("1080p") {
        return 0;
    }
    if res_match("720p") {
        return 1;
    }
    if res_match("480p") {
        return 2;
    }
    if res_match("2160p")
        || lower.contains("4k")
        || lower.contains(" uhd")
        || lower.contains(".uhd")
    {
        return 3;
    }
    4
}

fn is_hevc_from_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    // Collapse any separator between "h" and "265" / "264": space, dot, dash, etc.
    // "h 265", "h.265", "h-265", "h265" all normalize to "h265".
    let collapsed: String = {
        let mut out = String::with_capacity(lower.len());
        let chars: Vec<char> = lower.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            // Look for a "h" followed by separator(s) followed by "265"/"264"
            if chars[i] == 'h' && i + 1 < chars.len() {
                let mut j = i + 1;
                while j < chars.len() && matches!(chars[j], ' ' | '.' | '-' | '_') {
                    j += 1;
                }
                if j + 2 < chars.len()
                    && chars[j] == '2'
                    && chars[j + 1] == '6'
                    && (chars[j + 2] == '5' || chars[j + 2] == '4')
                {
                    out.push('h');
                    out.push('2');
                    out.push('6');
                    out.push(chars[j + 2]);
                    i = j + 3;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    };

    collapsed.contains("x265")
        || collapsed.contains("h265")
        || collapsed.contains("hevc")
        || collapsed.contains("10bit")
        || collapsed.contains("10-bit")
}

/// Detect Dolby Vision profile marker in torrent filename.
///
/// Dolby Vision profiles 5 and 7 embed an RPU NAL unit that NVENC on
/// Darwin's GTX 1650 cannot decode cleanly. ffmpeg logs "Error parsing DOVI
/// NAL unit" and "RPU validation failed: 0 <= el_bit_depth_minus8 = 32 <= 8"
/// for every frame and the transcode crawls at <1x realtime, which can't
/// sustain a live Chromecast stream. Until/unless we get a GPU that handles
/// DV RPU (or implement a `--strip-dolby-vision` ffmpeg pre-pass), we
/// demote DV titles below their non-DV siblings in the ranker.
///
/// Detection is token-based to avoid matching "DVD" as "DV". Common markers:
/// `DV`, `DoVi`, `Dolby Vision`, `Dolby.Vision`, `DV.P5`, `DV.P7`.
fn has_dolby_vision_in_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    if lower.contains("dolby vision")
        || lower.contains("dolby.vision")
        || lower.contains("dolbyvision")
    {
        return true;
    }
    // Word-boundary check for "dv"/"dovi" so "DVD" / "DVDRip" don't match.
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == "dv" || tok == "dovi")
}

fn extract_episode(val: &Value) -> Option<EpisodeRef> {
    if val.is_null() {
        return None;
    }
    Some(EpisodeRef {
        season: val["season_number"].as_u64()? as u32,
        episode: val["episode_number"].as_u64()? as u32,
        name: val["name"].as_str().map(String::from),
        air_date: val["air_date"].as_str().map(String::from),
    })
}

/// Apr 28, 2026: Build a full TMDB poster URL from a `poster_path` field.
///
/// TMDB's `/movie/{id}` and `/tv/{id}` endpoints return `poster_path` as a
/// relative segment like `"/qZQqEgXgGRpC8nJa9j5ej31Ynmm.jpg"`; the consumer
/// is expected to prefix it with the image base URL + a size descriptor.
/// We pick `w500` because it's the sweet spot for Cast UI rendering: large
/// enough for the receiver's full-screen poster background on a 1080p TV
/// without paying for unused pixels (`original` is often 2-4 MB; `w500`
/// is ~80-120 KB and indistinguishable at TV viewing distance).
///
/// Defenses:
///   - `None` / empty → `None` (no synthetic URL)
///   - Already-prefixed URL (someone manually built one) → returned as-is
///     so we don't end up with `https://image.tmdb.org/t/p/w500https://...`
///   - Missing leading slash → still works, we insert one
/// Web-remote T-4: strip scene-release noise so the TMDB query is the bare work title. Pure, RED-GREEN tested; boundary-only (never mutates `LibraryEntry.title`, so play/bypass is intact).
fn clean_title_for_tmdb(raw: &str) -> String {
    // Truncate at the FIRST scene "stop token" after normalising `._-`
    // to spaces. Unambiguous markers (SxxExx / Sxx / NxNN / NNNp / WxH)
    // stop anywhere; a 4-digit year and the source/codec/tag keyword set
    // only stop once real title content precedes them, so a title that
    // legitimately opens with such a word ("Web of Lies", "2001 A Space
    // Odyssey") survives. Empty result => caller falls back to raw title.
    fn is_stop(tok: &str, have_content: bool) -> bool {
        let t = tok.to_ascii_lowercase();
        let b = t.as_bytes();
        if b.is_empty() {
            return false;
        }
        // Sxx / SxxExx (S01, S07E19) — unambiguous, stops anywhere.
        if b[0] == b's' && b.len() >= 2 && b[1].is_ascii_digit() {
            return true;
        }
        // NxNN episode (1x06) OR WxH resolution (1920x1038) — unambiguous.
        if let Some(x) = t.find('x') {
            let (l, r) = (&t[..x], &t[x + 1..]);
            if !l.is_empty()
                && !r.is_empty()
                && l.bytes().all(|c| c.is_ascii_digit())
                && r.bytes().all(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
        // Resolution 720p/1080p/2160p — unambiguous.
        if t.len() >= 3 && t.ends_with('p') && t[..t.len() - 1].bytes().all(|c| c.is_ascii_digit())
        {
            return true;
        }
        if !have_content {
            return false;
        }
        // 4-digit release year — only once a title precedes it.
        if t.len() == 4
            && t.bytes().all(|c| c.is_ascii_digit())
            && (t.starts_with("19") || t.starts_with("20"))
        {
            return true;
        }
        matches!(
            t.as_str(),
            "bluray"
                | "blu"
                | "brrip"
                | "bdrip"
                | "dvdrip"
                | "web"
                | "webdl"
                | "webrip"
                | "hdtv"
                | "hdrip"
                | "remux"
                | "x264"
                | "x265"
                | "h264"
                | "h265"
                | "hevc"
                | "xvid"
                | "divx"
                | "aac"
                | "ac3"
                | "dts"
                | "flac"
                | "dd5"
                | "ddp5"
                | "limited"
                | "proper"
                | "rerip"
                | "repack"
                | "unrated"
                | "extended"
                | "remastered"
                | "theatrical"
                | "internal"
                | "3d"
                | "sbs"
                | "hsbs"
                | "mkvonly"
                | "multilang"
                | "multisub"
                | "multilingual"
                | "dubbed"
                | "subbed"
                | "complete"
                | "season"
        )
    }
    let normalized: String = raw
        .chars()
        .map(|c| {
            if c == '.' || c == '_' || c == '-' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out: Vec<&str> = Vec::new();
    for tok in normalized.split_whitespace() {
        if is_stop(tok, !out.is_empty()) {
            break;
        }
        out.push(tok);
    }
    out.join(" ").trim().to_string()
}

/// Web-remote T-4: normalise a title for similarity comparison — lowercase, every non-alphanumeric run to a single space, trimmed. Pure.
fn title_norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // leading-space suppression
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Web-remote T-4: char-level Levenshtein edit distance (unicode-safe; titles are short so O(n*m) is trivial). Pure.
fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Web-remote T-4: orthographic similarity in [0,1] = 1 - lev/maxlen. 1.0 = identical; ~0.93 for a single-char typo in a ~15-char title; garbage scores well under the ACCEPT floor. Pure.
fn title_similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let max = ac.len().max(bc.len());
    if max == 0 {
        return 0.0;
    }
    1.0 - (levenshtein(&ac, &bc) as f32 / max as f32)
}

/// Web-remote T-4: stopword-filtered asymmetric token containment — fraction of meaningful `query` words that fuzzy-appear (per-word char-sim >= 0.80) among `cand` words. 1.0 = every meaningful query word matched (the canonical title may add more). Stopwords ({the,of,a,an,and,to,in,on,&}) are dropped so common-word coincidence ("the godfather"~"the matrix") doesn't score; if a side is all-stopwords it falls back unfiltered (titles like "It"/"Up"). Right metric for release-cleaned-query vs canonical-title — tolerates typos AND length/subset differences. Pure.
fn title_token_score(query: &str, cand: &str) -> f32 {
    fn toks(s: &str) -> Vec<&str> {
        const STOP: [&str; 9] = ["the", "of", "a", "an", "and", "to", "in", "on", "&"];
        let all: Vec<&str> = s.split_whitespace().collect();
        let kept: Vec<&str> = all.iter().copied().filter(|w| !STOP.contains(w)).collect();
        if kept.is_empty() {
            all
        } else {
            kept
        }
    }
    let q = toks(query);
    if q.is_empty() {
        return 0.0;
    }
    let c = toks(cand);
    let hits = q
        .iter()
        .filter(|qt| c.iter().any(|ct| title_similarity(qt, ct) >= 0.80))
        .count();
    hits as f32 / q.len() as f32
}

/// 2026-06-09: Extract a 4-digit release year (1900-2099) from a TMDB query,
/// returning `(cleaned_query, Some(year))`. Matches both bare (`Spring 2014`)
/// and parenthesised (`Spring (2014)`) forms. The cleaned query has the year
/// token (and any surrounding parens/whitespace) stripped so the API call
/// goes out as the bare title — TMDB's `/search/movie` matches title text,
/// not "title plus year", so leaving the year in the query degrades recall.
/// Returns `(query.to_string(), None)` when no year is present.
///
/// Pure — testable without TMDB.
fn extract_year_from_query(query: &str) -> (String, Option<u32>) {
    // Scan for a 4-digit token whose value is 1900..=2099, allowing
    // surrounding `(` / `)` / whitespace. Use the FIRST match: a query like
    // "2001 A Space Odyssey 1968" is rare and ambiguous; first-match prefers
    // titles-that-open-with-a-year less than they'd suffer.
    let bytes = query.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        // Find a digit run of exactly 4
        let is_digit = |b: u8| b.is_ascii_digit();
        let prev_boundary = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'(' | b'[' | b'{' | b'-' | b'.'
            );
        if prev_boundary && (0..4).all(|k| is_digit(bytes[i + k])) {
            let next_boundary = i + 4 == bytes.len()
                || matches!(
                    bytes[i + 4],
                    b' ' | b'\t' | b')' | b']' | b'}' | b'-' | b'.'
                );
            if next_boundary {
                let year_str = &query[i..i + 4];
                if let Ok(year) = year_str.parse::<u32>() {
                    if (1900..=2099).contains(&year) {
                        // Build cleaned query: strip the year, any surrounding
                        // parens, and collapse whitespace.
                        let mut start = i;
                        let mut end = i + 4;
                        // Eat a leading `(` / `[` / `{` if it abuts the year.
                        if start > 0 && matches!(bytes[start - 1], b'(' | b'[' | b'{') {
                            start -= 1;
                        }
                        // Eat a trailing `)` / `]` / `}` if it abuts the year.
                        if end < bytes.len() && matches!(bytes[end], b')' | b']' | b'}') {
                            end += 1;
                        }
                        let cleaned = format!("{} {}", &query[..start], &query[end..]);
                        // Collapse runs of whitespace.
                        let cleaned: String =
                            cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
                        return (cleaned, Some(year));
                    }
                }
            }
        }
        i += 1;
    }
    (query.to_string(), None)
}

/// 2026-06-09: Composite score for a TMDB search candidate (movie or tv),
/// in roughly `[0.0, 1.5]`. Combines three orthogonal signals:
///
///   - **title_token_score** (primary, 0.0-1.0): asymmetric token containment
///     against the query. Reuses the same scorer poster lookup uses.
///   - **year_bonus** (+0.20 exact / +0.10 within ±1): when the query carries
///     a year, candidates whose `release_date` / `first_air_date` matches
///     get a strong nudge. The ±1 tolerance covers release-name vs TMDB
///     primary-release-year off-by-ones (common for non-US releases).
///   - **vote_count_bonus** (+0.15 / +0.05 / 0.0): popularity signal that
///     distinguishes "real movie everyone knows" from "obscure same-named
///     short". Floor of 10 votes; logarithmic flattening above 100.
///
/// Pure. The thresholds are conservative: title token-match dominates;
/// the bonuses break ties between same-token-score candidates rather than
/// overriding a title mismatch.
fn score_tmdb_candidate(item: &Value, query_norm: &str, query_year: Option<u32>) -> f32 {
    let title = item["title"]
        .as_str()
        .or_else(|| item["name"].as_str())
        .or_else(|| item["original_title"].as_str())
        .or_else(|| item["original_name"].as_str())
        .unwrap_or("");
    let title_n = title_norm(title);
    let token = title_token_score(query_norm, &title_n);

    let item_year = item["release_date"]
        .as_str()
        .or_else(|| item["first_air_date"].as_str())
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<u32>().ok());
    let year_bonus = match (query_year, item_year) {
        (Some(q), Some(i)) if q == i => 0.20,
        (Some(q), Some(i)) if (q as i32 - i as i32).abs() <= 1 => 0.10,
        _ => 0.0,
    };

    let votes = item["vote_count"].as_u64().unwrap_or(0);
    let vote_bonus = if votes >= 100 {
        0.15
    } else if votes >= 10 {
        0.05
    } else {
        0.0
    };

    token + year_bonus + vote_bonus
}

/// 2026-06-09: Pick the best TMDB candidate from a slice. Returns `None`
/// when no candidate clears the confidence floor — the caller decides whether
/// to error out or fall back to a less-confident pick.
///
/// Floor logic: a candidate passes if `score >= 0.50` OR it has an exact
/// year-match. The 0.50 floor means at least half the meaningful query
/// tokens are matched; below that, even a popular result is more likely a
/// false friend than the intended title. Exact year-match overrides because
/// queries with year (e.g. `Spring 2014`) are unambiguous user intent.
///
/// Pure.
fn pick_best_tmdb_candidate(
    candidates: &[&Value],
    query: &str,
    query_year: Option<u32>,
) -> Option<Value> {
    let q_norm = title_norm(query);
    let mut best: Option<(f32, &Value)> = None;
    for c in candidates {
        let score = score_tmdb_candidate(c, &q_norm, query_year);
        if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
            best = Some((score, c));
        }
    }
    let (best_score, best_item) = best?;
    let item_year = best_item["release_date"]
        .as_str()
        .or_else(|| best_item["first_air_date"].as_str())
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<u32>().ok());
    let exact_year_match = matches!((query_year, item_year), (Some(q), Some(i)) if q == i);
    if best_score >= 0.50 || exact_year_match {
        Some((*best_item).clone())
    } else {
        None
    }
}

fn tmdb_poster_url(poster_path: Option<&str>) -> Option<String> {
    let path = poster_path?.trim();
    if path.is_empty() {
        return None;
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        return Some(path.to_string());
    }
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Some(format!("https://image.tmdb.org/t/p/w500{normalized}"))
}

fn parse_torrentio_title(title: &str) -> (u32, String, String) {
    let seeds = title
        .find("👤")
        .and_then(|i| title[i..].split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0);
    let size = title
        .find("💾")
        .and_then(|i| {
            let rest = &title[i + "💾".len()..];
            let parts: Vec<&str> = rest.trim().splitn(3, ' ').collect();
            if parts.len() >= 2 {
                Some(format!("{} {}", parts[0], parts[1]))
            } else {
                None
            }
        })
        .unwrap_or_default();
    let source = title
        .find("⚙️")
        .and_then(|i| {
            title[i + "⚙️".len()..]
                .trim()
                .split_whitespace()
                .next()
                .map(String::from)
        })
        .unwrap_or_default();
    (seeds, size, source)
}

fn build_magnet(info_hash: &str, name: &str) -> String {
    let trackers: String = PUBLIC_TRACKERS
        .iter()
        .map(|t| format!("&tr={}", urlencoded(t)))
        .collect();
    format!(
        "magnet:?xt=urn:btih:{}&dn={}{}",
        info_hash,
        urlencoded(name),
        trackers
    )
}

fn urlencoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Parse inline season/episode markers from a search query string.
///
/// Recognized forms (case-insensitive, ASCII-only):
///   - `S05E06` / `s05e06` / `S5E6` / `s5e6`           (preferred — most common)
///   - `5x06` / `5x6`                                   (alternative slash form)
///   - `season 5 episode 6` / `Season 5 Episode 6`     (verbal)
///   - `season 5` (no episode) / `episode 6` (no season) — captured individually
///
/// Returns `(cleaned_query, season, episode)` where `cleaned_query` is the
/// original query with the marker substring stripped (and adjacent
/// whitespace collapsed). If no marker matches, returns `(query.into(), None, None)`.
///
/// Defenses:
///   - Numeric range clamped to 1..=999 — wider than realistic seasons
///     (max real-world is ~70 e.g. The Simpsons) but tight enough that
///     a 4-digit number isn't accidentally claimed as an episode.
///   - The S/E and `NxN` forms must sit on token boundaries so we don't
///     mangle codec markers like `H.265` / `1080p` / `5.1` audio.
///   - The verbal form requires the literal words `season` or `episode`
///     so a query like `"5 6"` isn't parsed as S5E6.
///
/// Why this exists: see `SearchEngine::search` for the May 6, 2026
/// "endlessly proud about streaming" loop incident — Ruby's tool-loop
/// issued `spela search "The Boys S05E06"` (no flags) and TMDB choked
/// on the marker, returning empty results. Ruby narrated success
/// anyway. Pre-parsing the marker on the engine side makes the
/// natural-language form work first time.
pub(crate) fn parse_episode_markers(query: &str) -> (String, Option<u32>, Option<u32>) {
    use once_cell::sync::Lazy;
    use regex::Regex;

    // Order matters: try most specific patterns first. Each regex
    // returns Some((season, episode)) on first match.
    static RE_SXXEXX: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b[sS](\d{1,3})[eE](\d{1,3})\b").unwrap());
    static RE_NXM: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,3})x(\d{1,3})\b").unwrap());
    static RE_VERBAL_BOTH: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bseason\s+(\d{1,3})\s+(?:and\s+|,\s*)?episode\s+(\d{1,3})\b").unwrap()
    });
    static RE_VERBAL_SEASON_ONLY: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\bseason\s+(\d{1,3})\b").unwrap());
    static RE_VERBAL_EPISODE_ONLY: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\bepisode\s+(\d{1,3})\b").unwrap());

    let parse_clamped = |s: &str| -> Option<u32> {
        let n: u32 = s.parse().ok()?;
        if (1..=999).contains(&n) {
            Some(n)
        } else {
            None
        }
    };

    let strip_match = |q: &str, mat: regex::Match| -> String {
        let mut cleaned = String::with_capacity(q.len());
        cleaned.push_str(&q[..mat.start()]);
        cleaned.push_str(&q[mat.end()..]);
        // Collapse runs of whitespace introduced by the strip.
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    // 1. SxxExx (highest specificity).
    if let Some(caps) = RE_SXXEXX.captures(query) {
        let s = caps.get(1).and_then(|m| parse_clamped(m.as_str()));
        let e = caps.get(2).and_then(|m| parse_clamped(m.as_str()));
        if s.is_some() || e.is_some() {
            let mat = caps.get(0).unwrap();
            return (strip_match(query, mat), s, e);
        }
    }

    // 2. Verbal "season N (and )?episode M" — must come BEFORE NxM so
    //    "season 5 episode 6" isn't accidentally consumed by NxM.
    if let Some(caps) = RE_VERBAL_BOTH.captures(query) {
        let s = caps.get(1).and_then(|m| parse_clamped(m.as_str()));
        let e = caps.get(2).and_then(|m| parse_clamped(m.as_str()));
        if s.is_some() || e.is_some() {
            let mat = caps.get(0).unwrap();
            return (strip_match(query, mat), s, e);
        }
    }

    // 3. NxM — `5x06` / `5x6`. Lower-priority because it can match
    //    things like resolutions in unusual filenames (rare in TMDB
    //    queries but possible).
    if let Some(caps) = RE_NXM.captures(query) {
        let s = caps.get(1).and_then(|m| parse_clamped(m.as_str()));
        let e = caps.get(2).and_then(|m| parse_clamped(m.as_str()));
        if s.is_some() || e.is_some() {
            let mat = caps.get(0).unwrap();
            return (strip_match(query, mat), s, e);
        }
    }

    // 4. Verbal "season N" or "episode M" alone.
    let mut cleaned = query.to_string();
    let mut season_only: Option<u32> = None;
    let mut episode_only: Option<u32> = None;
    if let Some(caps) = RE_VERBAL_SEASON_ONLY.captures(&cleaned) {
        if let Some(s) = caps.get(1).and_then(|m| parse_clamped(m.as_str())) {
            season_only = Some(s);
            let mat = caps.get(0).unwrap();
            cleaned = strip_match(&cleaned, mat);
        }
    }
    if let Some(caps) = RE_VERBAL_EPISODE_ONLY.captures(&cleaned) {
        if let Some(e) = caps.get(1).and_then(|m| parse_clamped(m.as_str())) {
            episode_only = Some(e);
            let mat = caps.get(0).unwrap();
            cleaned = strip_match(&cleaned, mat);
        }
    }
    if season_only.is_some() || episode_only.is_some() {
        return (cleaned, season_only, episode_only);
    }

    (query.to_string(), None, None)
}

fn parse_tv_episode_intent(query: &str) -> (String, TvEpisodeIntent) {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static RE_LATEST_SEASON_FIRST: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)\b(?:(?:latest|newest)\s+season\s+(?:first|1st)\s+(?:episode|ep)|(?:first|1st)\s+(?:episode|ep)\s+of\s+(?:the\s+)?(?:latest|newest)\s+season)\b",
        )
        .unwrap()
    });
    static RE_LATEST_SEASON: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b(?:latest|newest)\s+season\b").unwrap());
    static RE_LATEST_EPISODE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b(?:latest|newest)(?:\s+(?:episode|ep))?\b").unwrap());
    static RE_FIRST_EPISODE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)\b(?:first|1st)\s+(?:episode|ep)\b").unwrap());

    fn strip_match(q: &str, mat: regex::Match) -> String {
        let mut cleaned = String::with_capacity(q.len());
        cleaned.push_str(&q[..mat.start()]);
        cleaned.push_str(&q[mat.end()..]);
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    let patterns = [
        (
            &*RE_LATEST_SEASON_FIRST,
            TvEpisodeIntent::LatestSeasonFirstEpisode,
        ),
        (
            &*RE_LATEST_SEASON,
            TvEpisodeIntent::LatestSeasonFirstEpisode,
        ),
        (&*RE_LATEST_EPISODE, TvEpisodeIntent::LatestEpisode),
        (&*RE_FIRST_EPISODE, TvEpisodeIntent::FirstEpisode),
    ];

    for (re, intent) in patterns {
        if let Some(mat) = re.find(query) {
            return (strip_match(query, mat), intent);
        }
    }

    (query.to_string(), TvEpisodeIntent::None)
}

fn episode_to_search(
    season: Option<u32>,
    episode: Option<u32>,
    intent: TvEpisodeIntent,
    latest_episode: Option<&EpisodeRef>,
) -> (u32, u32) {
    match intent {
        TvEpisodeIntent::LatestEpisode => {
            let latest_season = latest_episode.map(|e| e.season).unwrap_or(1);
            let latest_episode_num = latest_episode.map(|e| e.episode).unwrap_or(1);
            (
                season.unwrap_or(latest_season),
                episode.unwrap_or(latest_episode_num),
            )
        }
        TvEpisodeIntent::LatestSeasonFirstEpisode => {
            let latest_season = latest_episode.map(|e| e.season).unwrap_or(1);
            (season.unwrap_or(latest_season), episode.unwrap_or(1))
        }
        TvEpisodeIntent::FirstEpisode | TvEpisodeIntent::None => match (season, episode) {
            (Some(s), Some(e)) => (s, e),
            (Some(s), None) => (s, 1),
            (None, Some(e)) => (1, e),
            (None, None) => (1, 1),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Apr 28, 2026: tmdb_poster_url regression suite -----
    //
    // Pin the URL-construction logic so it can't silently regress and start
    // emitting bogus URLs the Default Media Receiver can't fetch. A wrong
    // URL in the LOAD message would land us back in the persistent-overlay
    // fallback (the very bug we're trying to fix).

    #[test]
    fn tmdb_poster_url_typical_path() {
        let url = tmdb_poster_url(Some("/qZQqEgXgGRpC8nJa9j5ej31Ynmm.jpg"));
        assert_eq!(
            url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/qZQqEgXgGRpC8nJa9j5ej31Ynmm.jpg")
        );
    }

    #[test]
    fn tmdb_poster_url_missing_leading_slash_is_inserted() {
        // TMDB always returns leading slash, but defend against a future
        // schema change or upstream bug where it doesn't.
        let url = tmdb_poster_url(Some("abc.jpg"));
        assert_eq!(
            url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/abc.jpg")
        );
    }

    #[test]
    fn tmdb_poster_url_already_full_url_passes_through() {
        // If someone (mistakenly or via cache migration) has stored a
        // already-prefixed URL, don't double-prefix it.
        let url = tmdb_poster_url(Some("https://example.com/poster.jpg"));
        assert_eq!(url.as_deref(), Some("https://example.com/poster.jpg"));

        let url = tmdb_poster_url(Some("http://example.com/poster.jpg"));
        assert_eq!(url.as_deref(), Some("http://example.com/poster.jpg"));
    }

    #[test]
    fn tmdb_poster_url_none_returns_none() {
        assert_eq!(tmdb_poster_url(None), None);
    }

    // T-4 movie_poster guard paths — deterministic, NO network (both
    // short-circuit before any TMDB call). The TMDB-hit path is
    // integration (network) and intentionally not unit-pinned; the
    // poster-URL shaping it depends on is covered by the
    // `tmdb_poster_url_*` tests above.
    #[tokio::test]
    async fn movie_poster_empty_key_is_none_no_network() {
        let e = SearchEngine::new(String::new());
        assert_eq!(e.movie_poster("Blade Runner", Some(1982)).await, None);
    }

    #[tokio::test]
    async fn movie_poster_blank_title_is_none_no_network() {
        let e = SearchEngine::new("dummy-key-never-used".into());
        assert_eq!(e.movie_poster("   ", None).await, None);
        assert_eq!(e.movie_poster("", Some(2000)).await, None);
    }

    // T-4 title cleaning — the dominant miss-cause was scene-noise in the
    // parsed title. Pure + deterministic; cases mirror the 2026-05-18
    // production misses (TV-with-SxxExx, untrimmed movie tags).
    #[test]
    fn clean_title_strips_tv_season_episode_noise() {
        assert_eq!(
            clean_title_for_tmdb("Black Mirror S01 720p HDTV x264"),
            "Black Mirror"
        );
        assert_eq!(
            clean_title_for_tmdb("Futurama S07E19 720p HDTV x264-EVOLVE"),
            "Futurama"
        );
        assert_eq!(
            clean_title_for_tmdb("Game Of Thrones S01 S02 S03 Complete 1080p X264 anoXmous"),
            "Game Of Thrones"
        );
    }

    #[test]
    fn clean_title_strips_movie_tags_and_dashes() {
        assert_eq!(
            clean_title_for_tmdb("Enter The Void LIMITED"),
            "Enter The Void"
        );
        assert_eq!(clean_title_for_tmdb("Pacific Rim 3D"), "Pacific Rim");
        assert_eq!(
            clean_title_for_tmdb("The Third Man-720p MP4 AAC x264 BRRip 1949-CC"),
            "The Third Man"
        );
    }

    #[test]
    fn clean_title_preserves_clean_titles_and_leading_year() {
        // No scene tokens → unchanged.
        assert_eq!(clean_title_for_tmdb("Aladdin"), "Aladdin");
        // A title that legitimately OPENS with a year/keyword survives
        // (year/keyword only stop once real content precedes them).
        assert_eq!(
            clean_title_for_tmdb("2001 A Space Odyssey 1080p BluRay"),
            "2001 A Space Odyssey"
        );
    }

    // T-4 typo-tolerance — orthographic similarity gates poster
    // acceptance. Pure, deterministic. Mirrors the real "American Psyco"
    // miss + the latent wrong-poster correctness concern.
    #[test]
    fn title_norm_strips_punct_and_lowercases() {
        assert_eq!(title_norm("American Psyco!!"), "american psyco");
        assert_eq!(title_norm("The.Third_Man  (1949)"), "the third man 1949");
        assert_eq!(title_norm("  Spirited—Away  "), "spirited away");
    }

    #[test]
    fn title_similarity_recovers_typo_rejects_garbage() {
        // Identical → 1.0 (the >= STRONG 0.90 fast-path).
        assert!((title_similarity("american psycho", "american psycho") - 1.0).abs() < 1e-6);
        // One-char typo in a ~15-char title → well above ACCEPT (0.72),
        // below STRONG (so it ranks rather than early-outs).
        let s = title_similarity("american psyco", "american psycho");
        assert!(s > 0.85 && s < 1.0, "typo sim was {s}");
        // Coincidental wrong film → far below ACCEPT → titled fallback,
        // never a confidently-wrong poster.
        assert!(
            title_similarity("american psyco", "the godfather") < 0.5,
            "garbage must score < ACCEPT"
        );
    }

    #[test]
    fn levenshtein_basic() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein(&c("abc"), &c("abc")), 0);
        assert_eq!(levenshtein(&c("abc"), &c("axc")), 1);
        assert_eq!(levenshtein(&c("psyco"), &c("psycho")), 1);
        assert_eq!(levenshtein(&c(""), &c("abc")), 3);
    }

    // T-4 token containment — the metric the poster gate actually uses.
    // Pins the three behaviours: typo recovery, SUBSET acceptance (the
    // 2026-05-18 51->47 regression: whole-string Levenshtein wrongly
    // rejected legit length differences), garbage rejection.
    #[test]
    fn title_token_score_typo_subset_and_garbage() {
        // Typo: both query words fuzzy-match → full containment.
        assert!(
            (title_token_score("american psyco", "american psycho") - 1.0).abs() < 1e-6,
            "typo must reach STRONG (1.0)"
        );
        // Subset: release-cleaned query ⊂ canonical title → 1.0 (the
        // regression this fixes — every query word appears).
        assert!(
            (title_token_score(
                "lord of the rings fellowship",
                "the lord of the rings the fellowship of the ring"
            ) - 1.0)
                .abs()
                < 1e-6,
            "query-subset-of-canonical must be full containment"
        );
        // True garbage → 0.0 (no meaningful word overlaps) → below the
        // hair-above-zero ACCEPT → titled fallback, never a wrong poster.
        assert_eq!(title_token_score("american psyco", "the godfather"), 0.0);
        // Stopword anti-coincidence: a shared "the" must NOT score —
        // otherwise the low ACCEPT would accept "The Matrix" for "The
        // Godfather". Meaningful words (godfather vs matrix) don't match.
        assert_eq!(title_token_score("the godfather", "the matrix"), 0.0);
        assert_eq!(title_token_score("", "anything"), 0.0);
    }

    #[test]
    fn tmdb_poster_url_empty_returns_none() {
        assert_eq!(tmdb_poster_url(Some("")), None);
        assert_eq!(
            tmdb_poster_url(Some("   ")),
            None,
            "Whitespace-only path must be rejected."
        );
    }

    #[test]
    fn tmdb_poster_url_handles_special_chars_in_path() {
        // TMDB poster paths are alphanumeric + dot, but defend against
        // unusual filenames (different image bucket, different content type).
        let url = tmdb_poster_url(Some("/some-name_with.dots-and_underscores.png"));
        assert_eq!(
            url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/some-name_with.dots-and_underscores.png")
        );
    }

    #[test]
    fn tmdb_poster_url_picks_w500_size() {
        // Pin the size choice so a future "let's use original" tweak that
        // bloats Cast LOAD messages by 20× gets caught by tests.
        let url = tmdb_poster_url(Some("/x.jpg")).unwrap();
        assert!(
            url.contains("/w500/"),
            "URL must include the w500 size descriptor: {url}"
        );
        assert!(
            !url.contains("/original/"),
            "Original-size posters are 2-4 MB, too heavy for the LOAD msg"
        );
    }

    #[test]
    fn tmdb_poster_url_show_info_serde_roundtrip() {
        // Apr 28, 2026: Adding a new optional field to ShowInfo shouldn't
        // break deserialization of pre-Apr-28 cached last_search.json files.
        // Pin the back-compat property here so a #[serde(default)] regression
        // is immediately visible.
        let legacy_json = r#"{
            "tmdb_id": 12345,
            "title": "Hijack"
        }"#;
        let info: ShowInfo =
            serde_json::from_str(legacy_json).expect("legacy ShowInfo must deserialize");
        assert_eq!(info.title, "Hijack");
        assert!(
            info.poster_url.is_none(),
            "Missing poster_url field in old JSON must default to None."
        );
    }

    #[test]
    fn test_is_hevc_from_title() {
        assert!(is_hevc_from_title("Movie.2025.1080p.BluRay.x265-GROUP.mkv"));
        assert!(is_hevc_from_title("Movie.HEVC.1080p.mkv"));
        assert!(is_hevc_from_title("Movie.H265.mkv"));
        assert!(is_hevc_from_title("Movie.H.265.mkv"));
        assert!(is_hevc_from_title("Movie.10Bit.DDP5.1.mkv"));
        assert!(is_hevc_from_title("Movie.10-bit.mkv"));
        assert!(!is_hevc_from_title(
            "Movie.2025.1080p.BluRay.x264-GROUP.mkv"
        ));
        assert!(!is_hevc_from_title("Movie.H264.AAC.mp4"));
        assert!(!is_hevc_from_title("Movie.mp4"));
    }

    #[test]
    fn test_is_hevc_from_title_space_separated_variants() {
        // Apr 15, 2026 regression guard: `playWEB` and other release groups
        // format codec names with literal spaces. Real example from the
        // Apr 15 incident: "The Boys S05E02 Teenage Kix 2160p AMZN WEB-DL
        // DDP5 1 Atmos H 265-playWEB.mkv" was ranked #1 above a 1080p
        // H.264 release because the old codec detector missed "H 265".
        assert!(
            is_hevc_from_title(
                "The Boys S05E02 Teenage Kix 2160p AMZN WEB-DL DDP5 1 Atmos H 265-playWEB.mkv"
            ),
            "space-separated 'H 265' must register as HEVC"
        );
        assert!(is_hevc_from_title("Movie H 265 playWEB.mkv"));
        assert!(is_hevc_from_title("Movie H-265 playWEB.mkv"));
        assert!(is_hevc_from_title("Movie h 265.mkv"));
        // H 264 variants must NOT register as HEVC.
        assert!(
            !is_hevc_from_title(
                "The Boys S05E02 Teenage Kix 1080p AMZN WEB-DL DDP5 1 Atmos H 264-playWEB.mkv"
            ),
            "space-separated 'H 264' must NOT register as HEVC"
        );
        assert!(!is_hevc_from_title("Movie H 264 playWEB.mkv"));
        assert!(!is_hevc_from_title("Movie h 264.mkv"));
    }

    #[test]
    fn test_has_dolby_vision_in_title() {
        // Real problematic filename from the failed Boys S05E01 play
        assert!(has_dolby_vision_in_title(
            "The Boys S05E01 Fifteen Inches of Sheer Dynamite 2160p AMZN WEB-DL DDP5 1 Atmos DV HDR H 265-FLUX"
        ));
        // Common DV markers in various punctuation styles
        assert!(has_dolby_vision_in_title("Movie.2160p.DV.HDR.HEVC.mkv"));
        assert!(has_dolby_vision_in_title("Movie.2160p.DV.P7.HEVC.mkv"));
        assert!(has_dolby_vision_in_title("Movie.DoVi.HDR.HEVC.mkv"));
        assert!(has_dolby_vision_in_title("Movie.Dolby.Vision.HDR.mkv"));
        assert!(has_dolby_vision_in_title("Movie Dolby Vision HDR.mkv"));
        // Word-boundary check — DVD and DVDRip must NOT match
        assert!(!has_dolby_vision_in_title("Movie.2025.DVD.x264.mkv"));
        assert!(!has_dolby_vision_in_title("Movie.2025.DVDRip.x264.mkv"));
        assert!(!has_dolby_vision_in_title("Movie.2025.DVD9.x264.mkv"));
        // No DV markers at all
        assert!(!has_dolby_vision_in_title(
            "Movie.2025.1080p.BluRay.x264.mkv"
        ));
        assert!(!has_dolby_vision_in_title(
            "Movie.2025.2160p.HEVC.HDR10.mkv"
        ));
    }

    #[test]
    fn test_parse_torrentio_title() {
        // Actual torrentio format uses emoji + space-separated fields
        let title = "Movie.mkv\n👤 42 💾 1.5 GB ⚙️ TorrentGalaxy";
        let (seeds, size, source) = parse_torrentio_title(title);
        assert_eq!(seeds, 42);
        assert_eq!(size, "1.5 GB");
        assert_eq!(source, "TorrentGalaxy");
    }

    #[test]
    fn test_parse_torrentio_title_missing_fields() {
        let (seeds, size, source) = parse_torrentio_title("No metadata here");
        assert_eq!(seeds, 0);
        assert_eq!(size, "");
        assert_eq!(source, "");
    }

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("hello world"), "hello%20world");
        assert_eq!(urlencoded("foo/bar"), "foo%2Fbar");
        assert_eq!(urlencoded("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn test_build_magnet() {
        let magnet = build_magnet("abc123", "Movie.mkv");
        assert!(magnet.starts_with("magnet:?xt=urn:btih:abc123&dn=Movie.mkv"));
        assert!(magnet.contains("&tr="));
    }

    fn make_result(id: usize, title: &str, seeds: u32, file_index: Option<u32>) -> TorrentResult {
        TorrentResult {
            id,
            quality: "1080p".into(),
            title: title.into(),
            seeds,
            size: "1 GB".into(),
            source: "Test".into(),
            magnet: "magnet:test".into(),
            info_hash: "test".into(),
            file_index,
        }
    }

    #[test]
    fn test_ranking_single_file_over_pack() {
        let mut results = vec![
            make_result(1, "Movie.x264.mkv", 100, Some(3)), // pack
            make_result(2, "Movie.x264.mkv", 50, Some(0)),  // single file
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].seeds, 50); // single file wins despite fewer seeds
    }

    #[test]
    fn test_ranking_h264_over_hevc_with_enough_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)), // HEVC, well-seeded
            make_result(2, "Movie.x264.mkv", 20, Some(0)),  // H.264, decent seeds
        ];
        rank_results_mut(&mut results);
        // v3.1.0: tier 4 (H.264 > HEVC) still applies when both are the
        // same resolution (here both unknown). H.264 wins because 20 ≥ 5
        // threshold — same behavior as v3.0.0 for this fixture.
        assert_eq!(results[0].title, "Movie.x264.mkv");
    }

    #[test]
    fn test_ranking_h264_at_exact_threshold() {
        // Exactly 5 seeds = threshold met, H.264 should win (tier 4)
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)),
            make_result(2, "Movie.x264.mkv", 5, Some(0)), // exactly at threshold
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].title, "Movie.x264.mkv");
    }

    #[test]
    fn test_ranking_both_h264_sorts_by_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x264.FLEET.mkv", 10, Some(0)),
            make_result(2, "Movie.x264.YTS.mp4", 50, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].seeds, 50); // tier 5 seed tiebreak within same codec tier
    }

    #[test]
    fn test_ranking_both_hevc_sorts_by_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x265.10Bit.mkv", 200, Some(0)),
            make_result(2, "Movie.HEVC.mkv", 50, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].seeds, 200);
    }

    #[test]
    fn test_is_hevc_case_insensitive() {
        assert!(is_hevc_from_title("Movie.X265.mkv"));
        assert!(is_hevc_from_title("Movie.HEVC.mkv"));
        assert!(is_hevc_from_title("Movie.H.265.MKV"));
    }

    #[test]
    fn test_ranking_well_seeded_hevc_over_dead_h264() {
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)), // HEVC, well-seeded
            make_result(2, "Movie.x264.mkv", 2, Some(0)),   // H.264, nearly dead
        ];
        rank_results_mut(&mut results);
        // H.264 has only 2 seeds, below the tier 4 viability threshold of 5,
        // so tier 4 doesn't fire and we fall through to tier 5 seed tiebreak:
        // 100 > 2 → HEVC wins. Same behavior as v3.0.0 for this fixture.
        assert_eq!(results[0].title, "Movie.x265.mkv");
    }

    // --- Seed-disparity override (May 13, 2026 v3.4.0) ---
    //
    // Apr 13, 2026 (sic — May 13) incident: searching `The Boys` S05E07
    // returned a Cinecalidad 1080p H.264 release with 99 seeds as the
    // tier-4 winner ahead of a MeGusta 1080p HEVC release with 7116 seeds
    // (72× more). librqbit couldn't fetch the H.264 swarm's first piece
    // before ffmpeg's reconnect budget expired (50 bytes + EBML parse
    // failure + 0 segments + 75 s blue-cast icon). The MeGusta release
    // would have started streaming within seconds. The fix: when the HEVC
    // alternative has ≥30× the seeds of the H.264 tier-4 winner, override
    // the codec preference. Rationale: 1080p HEVC→H.264 NVENC transcode
    // on Darwin's GTX 1650 adds 5-10 s of cold-start cost; a starved
    // swarm adds *minutes* (or fails entirely). The latency tradeoff
    // flips.

    #[test]
    fn test_ranking_may13_2026_apr13_incident_cinecalidad_vs_megusta() {
        // Exact fixture from the May 13 2026 The Boys S05E07 incident.
        // Cinecalidad H.264 1080p with 99 seeds was tier-4 winner under
        // v3.1.0 even though MeGusta HEVC 1080p had 7116 seeds (72×).
        // Under v3.4.0 disparity override, MeGusta must win.
        let mut results = vec![
            make_result(1, "The.Boys.S05E07.2026.WEB-DL.1080p-Dual-Lat", 99, None),
            make_result(
                2,
                "The.Boys.S05E07.The.Frenchman.the.Female.and.the.Man.Called.Mothers.Milk.1080p.HEVC.x265-MeGusta[EZTVx.to].mkv",
                7116,
                Some(0),
            ),
        ];
        rank_results_mut(&mut results);
        assert!(
            is_hevc_from_title(&results[0].title),
            "HEVC release with 72× more seeds should win the v3.4.0 disparity override — got {:?}",
            results[0].title
        );
        assert_eq!(results[0].seeds, 7116);
    }

    #[test]
    fn test_ranking_seed_disparity_at_exact_30x_threshold() {
        // Boundary: 30× exactly should trigger the override (≥, not >).
        let mut results = vec![
            make_result(1, "Movie.1080p.x264.mkv", 100, Some(0)),
            make_result(2, "Movie.1080p.x265.mkv", 3000, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            is_hevc_from_title(&results[0].title),
            "30× disparity is exactly the threshold — HEVC should win"
        );
    }

    #[test]
    fn test_ranking_seed_disparity_just_below_30x_keeps_h264_preference() {
        // 29.99× — codec preference still wins. Pins the boundary on the
        // other side so the threshold doesn't drift silently if the
        // constant is touched.
        let mut results = vec![
            make_result(1, "Movie.1080p.x264.mkv", 100, Some(0)),
            make_result(2, "Movie.1080p.x265.mkv", 2999, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            !is_hevc_from_title(&results[0].title),
            "29.99× disparity — H.264 preference still wins; got {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_seed_disparity_does_not_override_resolution_tier() {
        // Disparity check is INSIDE tier 4 (same resolution). A 720p HEVC
        // with massive seeds must NOT promote over a viable 1080p H.264.
        // Tier 3 (resolution) fires before tier 4 disparity check.
        let mut results = vec![
            make_result(1, "Movie.1080p.x264.mkv", 100, Some(0)),
            make_result(2, "Movie.720p.x265.mkv", 10000, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "1080p H.264 with viable seeds beats 720p HEVC regardless of swarm — got {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_seed_disparity_does_not_override_dv_gate() {
        // Tier 2 (non-DV > DV) is a HARD GPU gate. Even a massive seed
        // advantage on a DV release cannot promote it over a non-DV
        // alternative (NVENC on Darwin's GTX 1650 can't decode DV RPU).
        let mut results = vec![
            make_result(1, "Movie.1080p.DV.HEVC.mkv", 10000, Some(0)),
            make_result(2, "Movie.1080p.x264.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            !has_dolby_vision_in_title(&results[0].title),
            "DV gate must fire before disparity check — got {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_seed_disparity_handles_zero_seed_h264() {
        // Edge case: H.264 has 0 seeds. Any positive HEVC seed count
        // satisfies "≥30× more". The `max(1)` saturating-multiply guard
        // prevents division-by-zero / overflow at this boundary.
        let mut results = vec![
            make_result(1, "Movie.1080p.x264.mkv", 0, Some(0)),
            make_result(2, "Movie.1080p.x265.mkv", 50, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            is_hevc_from_title(&results[0].title),
            "HEVC with any positive seeds beats H.264 with 0 seeds — got {:?}",
            results[0].title
        );
    }

    // --- Transitivity guard (May 13, 2026 v3.4.1) ---
    //
    // The May 13 PM Night Manager S02E05 search exposed a non-transitive sort
    // comparator in v3.4.0's `rank_results_mut`. With three results:
    //   A = (1080p, HEVC, 65 seeds)  — single-file
    //   B = (720p,  H.264, 51 seeds) — single-file
    //   C = (1080p, H.264, 17 seeds) — single-file
    //
    // The pairwise tier decisions form a 3-cycle:
    //   A > B  (tier 3: 1080p > 720p, A has ≥50-seed viability)
    //   C > A  (tier 4: H.264 > HEVC, 17 ≥ 5, disparity 3.8× < 30×)
    //   B > C  (tier 3 skips — C is 1080p but only 17 seeds, below viability;
    //            tier 4 skips — same codec; tier 5: 51 > 17)
    //
    // Rust's `sort_by` requires a TOTAL order from the comparator; non-transitive
    // input → undefined output. v3.4.0 happened to land on [A, B, C] which
    // violates tier 4 between A and C.
    //
    // Root cause: tier 3's seed-viability gate is asymmetric — it ONLY checks
    // the higher-resolution operand's seeds. When the higher-res operand is
    // below the viability floor, tier 3 falls through entirely, and tier 5
    // can let a lower-res-but-better-seeded result outrank it. But tier 3
    // doesn't "remember" that fall-through when comparing two SAME-resolution
    // results where one is viable and one isn't.
    //
    // Fix (v3.4.1): collapse "resolution_tier + viability" into a single
    // `effective_res_tier` value at tier 3. Unviable seeds at a target
    // resolution are demoted into a lower bucket so they sort BELOW any viable
    // lower-resolution result. Total ordering restored.

    #[test]
    fn test_ranking_no_cycle_on_may13_s02e05_fixture() {
        // The exact 3-way comparator cycle from the May 13 PM
        // Night Manager S02E05 incident. After v3.4.1, the sort must
        // produce a TRANSITIVE ordering — A > B > C is the deterministic
        // total-order result because C (17 seeds at 1080p) demotes below
        // viable 720p (B) via `effective_res_tier`.
        let a = make_result(0, "NM.S02E05.1080p.HEVC.10bit.mkv", 65, Some(0));
        let b = make_result(0, "NM.S02E05.720p.H.264.mkv", 51, Some(0));
        let c = make_result(0, "NM.S02E05.1080p.H.264.mkv", 17, Some(0));

        // Validate pairwise consistency: if X > Y and Y > Z, then X > Z.
        for (perm_a, perm_b, perm_c) in [
            (a.clone(), b.clone(), c.clone()),
            (a.clone(), c.clone(), b.clone()),
            (b.clone(), a.clone(), c.clone()),
            (b.clone(), c.clone(), a.clone()),
            (c.clone(), a.clone(), b.clone()),
            (c.clone(), b.clone(), a.clone()),
        ] {
            let mut results = vec![perm_a, perm_b, perm_c];
            rank_results_mut(&mut results);
            // The deterministic transitive result must be A first (1080p HEVC,
            // viable seeds), B second (720p H.264, viable), C last (1080p H.264
            // demoted because 17 seeds < 50 viability floor).
            assert!(
                results[0].title.contains("HEVC.10bit"),
                "Expected A (1080p HEVC viable) first; got {:?}",
                results[0].title
            );
            assert!(
                results[1].title.contains("720p"),
                "Expected B (720p viable) second; got {:?}",
                results[1].title
            );
            assert!(
                results[2].title.contains("1080p.H.264"),
                "Expected C (1080p H.264 unviable) last; got {:?}",
                results[2].title
            );
        }
    }

    #[test]
    fn test_effective_res_tier_classification() {
        // Pin the effective_res_tier value mapping. Lower = ranked higher.
        // 0..=2: 1080p/720p/480p with ≥50 seeds (target-resolution viable)
        // 3..=5: 1080p/720p/480p with <50 seeds (demoted below viable 480p)
        // 6: 2160p / 4K / UHD (always deprioritized — Sarpetorp policy)
        // 7: unknown / not classified
        let r1080_viable = make_result(0, "X.1080p.x264.mkv", 50, Some(0));
        let r1080_unviable = make_result(0, "X.1080p.x264.mkv", 49, Some(0));
        let r720_viable = make_result(0, "X.720p.x264.mkv", 100, Some(0));
        let r720_unviable = make_result(0, "X.720p.x264.mkv", 5, Some(0));
        let r480_viable = make_result(0, "X.480p.x264.mkv", 50, Some(0));
        let r2160 = make_result(0, "X.2160p.x265.mkv", 500, Some(0));
        let r_unknown = make_result(0, "X.no.resolution.tag.mkv", 1000, Some(0));

        assert_eq!(effective_res_tier(&r1080_viable), 0);
        assert_eq!(effective_res_tier(&r720_viable), 1);
        assert_eq!(effective_res_tier(&r480_viable), 2);
        assert_eq!(effective_res_tier(&r1080_unviable), 3);
        assert_eq!(effective_res_tier(&r720_unviable), 4);
        assert_eq!(effective_res_tier(&r2160), 6);
        assert_eq!(effective_res_tier(&r_unknown), 7);

        // Critical invariant: viable lower resolution beats unviable higher.
        assert!(effective_res_tier(&r720_viable) < effective_res_tier(&r1080_unviable));
        // 2160p with great seeds STILL ranks below any viable 1080p/720p/480p.
        assert!(effective_res_tier(&r2160) > effective_res_tier(&r480_viable));
    }

    #[test]
    fn test_ranking_existing_h264_preference_unchanged_when_no_disparity() {
        // Sanity pin: when the seed ratio is normal (here ~5×), the
        // existing H.264 > HEVC tier-4 preference still applies. Guards
        // against the disparity override silently regressing the common
        // case.
        let mut results = vec![
            make_result(1, "Movie.1080p.x265.mkv", 500, Some(0)),
            make_result(2, "Movie.1080p.x264.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            !is_hevc_from_title(&results[0].title),
            "5× disparity is below threshold — H.264 preference wins; got {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_dolby_vision_hevc_demoted_below_plain_hevc() {
        // Same resolution (2160p), same codec class (HEVC), similar seeds —
        // non-DV HEVC must win because NVENC can't decode DV RPU cleanly.
        // v3.1.0: tier 2 (non-DV > DV) fires first and picks the HDR10 release.
        let mut results = vec![
            make_result(1, "Movie.2160p.DV.HDR.HEVC.mkv", 800, Some(0)),
            make_result(2, "Movie.2160p.HDR10.HEVC.mkv", 600, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].title, "Movie.2160p.HDR10.HEVC.mkv");
    }

    #[test]
    fn test_ranking_h264_still_beats_dolby_vision_hevc() {
        // Real-world S05E01 search shape: a healthy 1080p H.264 release and
        // the problematic 2160p Dolby Vision HEVC release. The H.264 must win
        // unambiguously — under v3.1.0 this wins via tier 2 (non-DV > DV)
        // immediately. Under v3.0.0 it would have won via tier 2 (H.264 > HEVC)
        // → same outcome, different reason.
        let mut results = vec![
            make_result(
                1,
                "The.Boys.S05E01.2160p.AMZN.WEB-DL.DV.HDR.H.265-FLUX.mkv",
                783,
                Some(0),
            ),
            make_result(
                2,
                "The.Boys.S05E01.1080p.WEB-DL.DUAL.5.1.H.264.mkv",
                1433,
                Some(0),
            ),
        ];
        rank_results_mut(&mut results);
        assert_eq!(
            results[0].title,
            "The.Boys.S05E01.1080p.WEB-DL.DUAL.5.1.H.264.mkv"
        );
    }

    // --- Resolution preference tier (Apr 15, 2026 v3.0.0) ---
    //
    // These tests use the extracted `rank_results_mut` so they track
    // production exactly. Older tests above still inline the sort closure
    // (legacy tech debt); new tests should all go through `rank_results_mut`.

    // --- Show-title filter for spurious Torrentio results (Apr 15, 2026) ---

    #[test]
    fn test_extract_significant_tokens_strips_stop_words() {
        assert_eq!(
            extract_significant_tokens("The Boys"),
            vec!["boys".to_string()]
        );
        assert_eq!(
            extract_significant_tokens("Game of Thrones"),
            vec!["game".to_string(), "thrones".to_string()]
        );
        assert_eq!(
            extract_significant_tokens("The Lord of the Rings"),
            vec!["lord".to_string(), "rings".to_string()]
        );
        // No stop words at all
        assert_eq!(
            extract_significant_tokens("Breaking Bad"),
            vec!["breaking".to_string(), "bad".to_string()]
        );
        // Single significant token
        assert_eq!(extract_significant_tokens("Lost"), vec!["lost".to_string()]);
        // Punctuation split
        assert_eq!(
            extract_significant_tokens("Dr. Who"),
            vec!["dr".to_string(), "who".to_string()]
        );
        // Empty
        assert_eq!(extract_significant_tokens(""), Vec::<String>::new());
        // All stop words (edge case — filter should be skipped)
        assert_eq!(extract_significant_tokens("The"), Vec::<String>::new());
    }

    #[test]
    fn test_filter_drops_french_chef_for_the_boys_query() {
        // The Apr 15 live incident: searching "The Boys" S05E03 returned
        // a French Chef release as the top-seeded candidate. Token filter
        // must drop it because "boys" (the only significant token of
        // "The Boys") doesn't appear in the French Chef filename.
        let results = vec![
            make_result(
                1,
                "The.French.Chef.Season.05.03of20.Queen.of.Sheba.Cake.WEB-DL.x264.AAC.mp4",
                820,
                Some(0),
            ),
            make_result(
                2,
                "The.Boys.S05E03.Every.One.of.You.Sons.of.Bitches.1080p.AMZN.WEB-DL.H264.FLUX.mkv",
                513,
                Some(0),
            ),
        ];
        let filtered = filter_results_by_show_title(results, "The Boys");
        assert_eq!(filtered.len(), 1, "French Chef should have been dropped");
        assert!(
            filtered[0].title.contains("Boys"),
            "Remaining result should be the Boys release. Got: {:?}",
            filtered[0].title
        );
    }

    #[test]
    fn test_filter_fallback_when_everything_would_be_dropped() {
        // Safety net: if token filtering would leave zero results, return
        // the unfiltered list instead (with a warning). Better to give the
        // user SOMETHING to play than nothing at all.
        let results = vec![
            make_result(1, "Nothing.Matches.The.Query.mkv", 100, Some(0)),
            make_result(2, "Another.Release.mkv", 50, Some(0)),
        ];
        let filtered = filter_results_by_show_title(results, "Breaking Bad");
        assert_eq!(
            filtered.len(),
            2,
            "Filter should fall back to unfiltered when it would drop everything"
        );
    }

    #[test]
    fn test_filter_empty_token_set_returns_unfiltered() {
        // Show title that's all stop words (extremely unlikely, but
        // extract_significant_tokens might return empty). Filter should
        // skip itself rather than explode or drop everything.
        let results = vec![make_result(1, "Some.Random.Release.mkv", 100, Some(0))];
        let filtered = filter_results_by_show_title(results, "The");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_filter_multi_word_show_requires_all_significant_tokens() {
        // "Game of Thrones" → significant tokens [game, thrones]. A
        // release named "Game of Hearts" should be dropped (has "game"
        // but not "thrones"). A release named "Game.of.Thrones.S01E01"
        // should be kept.
        let results = vec![
            make_result(1, "Game.of.Thrones.S01E01.1080p.mkv", 500, Some(0)),
            make_result(2, "Game.of.Hearts.S01E01.mkv", 500, Some(0)),
        ];
        let filtered = filter_results_by_show_title(results, "Game of Thrones");
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].title.contains("Thrones"));
    }

    #[test]
    fn test_filter_passes_through_case_insensitive() {
        // Release names use varying case; match must be case-insensitive.
        let results = vec![make_result(1, "THE.BOYS.S05E03.1080P.MKV", 100, Some(0))];
        let filtered = filter_results_by_show_title(results, "The Boys");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_filter_handles_separator_variants() {
        // Torrent filenames use dots/spaces/dashes/underscores. Since
        // match is substring on lowercased title, `boys` should match in
        // any separator layout.
        let cases = vec![
            "The.Boys.S05E03.FLUX.mkv",
            "The Boys S05E03 FLUX.mkv",
            "The-Boys-S05E03-FLUX.mkv",
            "The_Boys_S05E03_FLUX.mkv",
        ];
        for c in cases {
            let results = vec![make_result(1, c, 100, Some(0))];
            let filtered = filter_results_by_show_title(results, "The Boys");
            assert_eq!(filtered.len(), 1, "failed to keep {:?}", c);
        }
    }

    #[test]
    fn test_resolution_tier_ordering_v31_policy() {
        // v3.1.0 policy: 1080p is the sweet spot, 2160p demoted below 480p
        // because Sarpetorp TVs can't display 2160p natively and 4K NVENC
        // transcode is wasted effort + bandwidth.
        assert_eq!(resolution_tier("Movie.1080p.WEB-DL.mkv"), 0);
        assert_eq!(resolution_tier("The Boys S05E03 1080p AMZN"), 0);
        assert_eq!(resolution_tier("Movie.720p.WEB-DL.mkv"), 1);
        assert_eq!(resolution_tier("Movie.480p.x264.mkv"), 2);
        assert_eq!(resolution_tier("Movie.2160p.HDR.x265.mkv"), 3);
        assert_eq!(resolution_tier("Movie.4K.HDR.mkv"), 3);
        assert_eq!(resolution_tier("Movie.UHD.BluRay.mkv"), 3);
        assert_eq!(resolution_tier("Movie.No.Resolution.Tag.mkv"), 4);
        // Space-separated (playWEB convention)
        assert_eq!(resolution_tier("The Boys S05E03 1080 p AMZN"), 0);
        // Sanity: 1080p ranks strictly better than 2160p
        assert!(
            resolution_tier("Movie.1080p.mkv") < resolution_tier("Movie.2160p.mkv"),
            "v3.1.0: 1080p must rank higher than 2160p"
        );
        // Sanity: 2160p is dead-last among known resolutions
        assert!(
            resolution_tier("Movie.2160p.mkv") > resolution_tier("Movie.480p.x264.mkv"),
            "v3.1.0: 2160p must rank BELOW 480p (wasted on 1080p TVs)"
        );
    }

    #[test]
    fn test_ranking_prefers_1080p_h264_over_720p_h264_with_viable_seeds() {
        // Apr 15 incident: S05E03 search returned 720p H.264 (734 seeds)
        // above 1080p H.264 (513 seeds). With the resolution tier, 1080p
        // wins because 513 ≥ 50 viable threshold.
        let mut results = vec![
            make_result(
                1,
                "The Boys S05E03 Every One of You Sons of Bitches 720p AMZN WEB-DL H 264-FLUX.mkv",
                734,
                Some(0),
            ),
            make_result(
                2,
                "The Boys S05E03 Every One of You Sons of Bitches 1080p AMZN WEB-DL H 264-FLUX.mkv",
                513,
                Some(0),
            ),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "1080p H.264 with 513 seeds should outrank 720p H.264 with 734 seeds. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_falls_back_to_720p_when_1080p_has_insufficient_seeds() {
        // Threshold is 50 seeds for resolution preference. A dead 1080p
        // (10 seeds) must LOSE to a well-seeded 720p (500 seeds) because
        // the dead 1080p would stall mid-download.
        let mut results = vec![
            make_result(1, "Movie.720p.H264.FLUX.mkv", 500, Some(0)),
            make_result(2, "Movie.1080p.H264.FLUX.mkv", 10, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("720p"),
            "720p with 500 seeds must beat 1080p with 10 seeds. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_prefers_1080p_at_exact_50_seed_threshold() {
        // Boundary: 50 seeds = the viable threshold, 1080p wins.
        let mut results = vec![
            make_result(1, "Movie.720p.H264.mkv", 200, Some(0)),
            make_result(2, "Movie.1080p.H264.mkv", 50, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "1080p at exact 50 seeds should still win over 720p 200. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_hevc_1080p_beats_h264_720p_v31_policy() {
        // v3.1.0 POLICY INVERSION from v3.0.0. The old tier 2 (H.264 > HEVC)
        // sat above tier 4 (resolution), so 720p H.264 won over 1080p HEVC.
        // v3.1.0 swaps them: resolution is tier 3, codec is tier 4. The
        // reasoning: HEVC → H.264 transcode on Darwin's GTX 1650 at 1080p
        // runs at ~6x realtime (fast enough to absorb the cold-start cost),
        // so the quality benefit of 1080p HEVC beats the insta-play benefit
        // of 720p H.264 on a 1080p TV.
        let mut results = vec![
            make_result(1, "Movie.720p.H264.FLUX.mkv", 500, Some(0)),
            make_result(2, "Movie.1080p.HEVC.x265.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "v3.1.0: HEVC 1080p (100 seeds) must beat H.264 720p (500 seeds) — resolution tier 3 now dominates codec tier 4. Got: {:?}",
            results[0].title
        );
        assert!(
            results[0].title.contains("HEVC") || results[0].title.contains("x265"),
            "Got wrong release: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_1080p_h264_beats_2160p_h264_v31_policy() {
        // v3.1.0: 2160p is demoted to tier 3 rank 3 (below 1080p, 720p,
        // 480p) because Sarpetorp TVs max at 1080p native. A 1080p H.264
        // release with 100 seeds must beat a 2160p H.264 release with
        // 500 seeds because 2160p is wasted on a 1080p display AND the
        // 4K transcode is unnecessarily heavy.
        let mut results = vec![
            make_result(1, "Movie.2160p.H264.mkv", 500, Some(0)),
            make_result(2, "Movie.1080p.H264.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "v3.1.0: 1080p H.264 (100 seeds) must beat 2160p H.264 (500 seeds) — 2160p demoted below 1080p. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_2160p_demoted_below_720p() {
        // v3.1.0: 2160p is dead-last, so even 720p outranks it when both
        // are viable. Scenario: user has 2160p HEVC 1000 seeds AND 720p
        // HEVC 100 seeds. Both are non-DV, both HEVC (tier 4 ties). Tier 3
        // says 720p (1) < 2160p (3) → 720p wins.
        let mut results = vec![
            make_result(1, "Movie.2160p.HEVC.x265.mkv", 1000, Some(0)),
            make_result(2, "Movie.720p.HEVC.x265.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("720p"),
            "v3.1.0: 720p should beat 2160p (same codec class, 2160p demoted). Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_2160p_demoted_below_480p() {
        // v3.1.0 aggressive policy: 2160p sits BELOW even 480p because
        // 2160p's only virtue is pixel count which the TV discards, plus
        // it costs 3x more transcode effort. A 480p at 100 seeds beats
        // a 2160p at 1000 seeds. Safe because 2160p is usually HEVC and
        // often DV — this test forces a non-DV same-codec case to isolate
        // the resolution tier behavior.
        let mut results = vec![
            make_result(1, "Movie.2160p.x264.mkv", 1000, Some(0)),
            make_result(2, "Movie.480p.x264.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("480p"),
            "v3.1.0: 480p must outrank 2160p — same codec, 2160p demoted to last. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_dv_gate_fires_before_resolution_tier() {
        // v3.1.0 tier 2 is non-DV > DV (promoted from v3.0.0 tier 3). This
        // MUST fire before the resolution tier (3), so a DV 1080p with
        // 500 seeds still loses to a non-DV 720p with 100 seeds — because
        // DV is a hard GPU incompatibility on the GTX 1650, not a quality
        // preference. If the tier order were reversed (resolution first),
        // we'd pick the DV 1080p and the transcode would collapse to
        // 0.937x realtime. This test pins the ordering explicitly.
        let mut results = vec![
            make_result(1, "Movie.1080p.DV.HDR.HEVC.mkv", 500, Some(0)),
            make_result(2, "Movie.720p.HEVC.x265.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("720p"),
            "DV gate (tier 2) must fire before resolution preference (tier 3). Got: {:?}",
            results[0].title
        );
        assert!(
            !results[0].title.contains("DV"),
            "DV release should never win against a non-DV alternative. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_same_resolution_h264_beats_hevc_as_tiebreak() {
        // Within the same resolution bucket, tier 4 (H.264 > HEVC) is still
        // meaningful — 1080p H.264 plays instantly, 1080p HEVC transcodes.
        // Both releases must be at the same resolution for tier 4 to fire
        // (otherwise tier 3 picks higher res first).
        let mut results = vec![
            make_result(1, "Movie.1080p.HEVC.x265.mkv", 500, Some(0)),
            make_result(2, "Movie.1080p.H264.FLUX.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("H264") || results[0].title.contains("x264"),
            "H.264 should win tiebreak against HEVC at same resolution (insta-play). Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_1080p_h264_beats_2160p_hevc_with_viable_seeds() {
        // Another tier-2-over-tier-4 case: a viable H.264 1080p wins
        // against a well-seeded HEVC 2160p because tier 2 insta-play.
        let mut results = vec![
            make_result(1, "Movie.2160p.HEVC.x265.mkv", 1000, Some(0)),
            make_result(2, "Movie.1080p.H264.FLUX.mkv", 200, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("H264") || results[0].title.contains("H 264"),
            "1080p H.264 must beat 2160p HEVC — H.264 insta-play wins over higher res + HEVC transcode. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_prefers_720p_over_480p_with_viable_seeds() {
        let mut results = vec![
            make_result(1, "Movie.480p.x264.mkv", 1000, Some(0)),
            make_result(2, "Movie.720p.x264.mkv", 100, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("720p"),
            "720p (100 seeds) should beat 480p (1000 seeds) — resolution tier wins. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_s05e03_live_fixture_1080p_wins() {
        // Exact fixture from the Apr 15 evening live search. Before the
        // resolution tier, the 720p FLUX at 734 seeds ranked #1, and
        // Fredrik had to manually pick result #4. Post-fix, the 1080p
        // FLUX at 513 seeds ranks #1 and Ruby's default `spela play 1`
        // gets 1080p automatically.
        let mut results = vec![
            make_result(
                1,
                "The Boys S05E03 Every One of You Sons of Bitches 720p AMZN WEB-DL DDP5 1 Atmos H 264-FLUX[EZTVx.to].mkv",
                734,
                Some(0),
            ),
            make_result(
                2,
                "The.Boys.S05E03.480p.x264-mSD[EZTVx.to].mkv",
                573,
                Some(0),
            ),
            make_result(
                3,
                "The Boys S05E03 Every One of You Sons of Bitches 1080p AMZN WEB-DL DDP5 1 Atmos H 264-FLUX[EZTVx.to].mkv",
                513,
                Some(0),
            ),
            make_result(
                4,
                "The.Boys.S05E03.MULTi.1080p.AMZN.WEB-DL.H264.DDP5.1.Atmos-K83.mkv",
                68,
                Some(0),
            ),
            make_result(
                5,
                "The.Boys.S05E03.MULTi.720p.AMZN.WEB-DL.H264.DDP5.1.Atmos-K83.mkv",
                56,
                Some(0),
            ),
        ];
        rank_results_mut(&mut results);
        assert!(
            results[0].title.contains("1080p"),
            "Apr 15 S05E03 live fixture: 1080p must rank first post-tier-4. Got: {:?}",
            results[0].title
        );
        // The primary FLUX 1080p (513 seeds) should beat the MULTi 1080p
        // (68 seeds) on the seed tiebreak within same resolution.
        assert!(
            results[0].title.contains("FLUX"),
            "Within the 1080p bucket, FLUX (513) beats MULTi (68) on seeds. Got: {:?}",
            results[0].title
        );
    }

    #[test]
    fn test_ranking_playweb_space_separated_codecs_apr15() {
        // Apr 15, 2026 regression scenario: the live `spela search 'The Boys'
        // --season 5 --episode 2` returned three results, all playWEB-style:
        //   #1: "The Boys S05E02 Teenage Kix 2160p AMZN WEB-DL DDP5 1 Atmos H 265-playWEB.mkv" (674 seeds)
        //   #2: "The.Boys.S05E02.FRENCH.WEBRip.x264.mp4" (609 seeds)
        //   #3: "The Boys S05E02 Teenage Kix 1080p AMZN WEB-DL DDP5 1 Atmos H 264-playWEB.mkv" (seeds?)
        //
        // The 2160p HEVC release was ranked #1 even though the Apr 15 4-tier
        // ranker says H.264 > HEVC. Root cause: `is_hevc_from_title` missed
        // the "H 265" variant (space between letter and number) and treated
        // the 4K release as H.264. Fixed by normalizing separators between
        // the codec family letter and version number. This integration-level
        // test pins the fix by running the actual ranker sort over the
        // actual release titles from the incident.
        let mut results = vec![
            make_result(
                1,
                "The Boys S05E02 Teenage Kix 2160p AMZN WEB-DL DDP5 1 Atmos H 265-playWEB.mkv",
                674,
                Some(0),
            ),
            make_result(
                2,
                "The Boys S05E02 Teenage Kix 1080p AMZN WEB-DL DDP5 1 Atmos H 264-playWEB.mkv",
                200,
                Some(0),
            ),
        ];
        rank_results_mut(&mut results);
        // v3.1.0: 1080p beats 2160p at tier 3 (1080p has 200 ≥ 50 seeds),
        // so the 1080p H 264 playWEB release wins regardless of the
        // space-separated codec tokenization. Under v3.0.0, same outcome
        // but via tier 2 (H.264 > HEVC, after the is_hevc normalization
        // fix). Both passes prove the fix.
        assert!(
            results[0].title.contains("1080p"),
            "Regression: space-separated 'H 265' bypassed the HEVC ranker tier. \
             Result 1 was {:?}, expected the 1080p H.264 release.",
            results[0].title
        );
        assert!(
            results[0].title.contains("H 264"),
            "Regression: 1080p release not ranked first, got {:?}",
            results[0].title
        );
    }

    // ============================================================
    // May 7, 2026: parse_episode_markers regression suite.
    // The May 6 "endlessly proud about streaming" Ruby loop traced
    // back to TMDB choking on `"The Boys S05E06"` — Gemini's tool
    // loop never figured out the --season/--episode flag form, so
    // every retry hit empty results. Pre-parsing the marker fixes
    // it for every shape the LLM might emit.
    // ============================================================

    #[test]
    fn parse_markers_canonical_sxxexx() {
        let (q, s, e) = parse_episode_markers("The Boys S05E06");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_lowercase_sxxexx() {
        let (q, s, e) = parse_episode_markers("the boys s05e06");
        assert_eq!(q, "the boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_one_digit_each() {
        let (q, s, e) = parse_episode_markers("Doctor Who S5E6");
        assert_eq!(q, "Doctor Who");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_three_digit_episode() {
        // Some long-running TV runs (Simpsons-class) push past 99 episodes/season.
        let (q, s, e) = parse_episode_markers("Long Show S01E123");
        assert_eq!(q, "Long Show");
        assert_eq!(s, Some(1));
        assert_eq!(e, Some(123));
    }

    #[test]
    fn parse_markers_nxm_form() {
        let (q, s, e) = parse_episode_markers("The Boys 5x06");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_verbal_both() {
        let (q, s, e) = parse_episode_markers("The Boys season 5 episode 6");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_verbal_with_and() {
        let (q, s, e) = parse_episode_markers("The Boys season 5 and episode 6");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_verbal_capitalized() {
        let (q, s, e) = parse_episode_markers("The Boys Season 5 Episode 6");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_verbal_season_only() {
        let (q, s, e) = parse_episode_markers("The Boys season 5");
        assert_eq!(q, "The Boys");
        assert_eq!(s, Some(5));
        assert_eq!(e, None);
    }

    #[test]
    fn parse_markers_verbal_episode_only() {
        // Rare but harmless — e.g. someone with a search context already on a show.
        let (q, _s, e) = parse_episode_markers("Pilot episode 1");
        // We don't try to be smart about whether "Pilot" should remain;
        // the cleaned query just has "episode 1" stripped.
        assert!(q.contains("Pilot"));
        assert_eq!(e, Some(1));
    }

    #[test]
    fn parse_markers_no_marker_passthrough() {
        let (q, s, e) = parse_episode_markers("The Boys");
        assert_eq!(q, "The Boys");
        assert_eq!(s, None);
        assert_eq!(e, None);
    }

    #[test]
    fn unspecified_tv_search_defaults_to_first_episode() {
        assert_eq!(
            episode_to_search(None, None, TvEpisodeIntent::None, None),
            (1, 1)
        );
    }

    #[test]
    fn season_only_search_defaults_to_first_episode_of_that_season() {
        assert_eq!(
            episode_to_search(Some(2), None, TvEpisodeIntent::None, None),
            (2, 1)
        );
    }

    #[test]
    fn episode_only_search_defaults_to_first_season() {
        assert_eq!(
            episode_to_search(None, Some(3), TvEpisodeIntent::None, None),
            (1, 3)
        );
    }

    #[test]
    fn parse_tv_intent_latest_episode() {
        let (q, intent) = parse_tv_episode_intent("Pantheon latest");
        assert_eq!(q, "Pantheon");
        assert_eq!(intent, TvEpisodeIntent::LatestEpisode);

        let (q, intent) = parse_tv_episode_intent("Pantheon newest episode");
        assert_eq!(q, "Pantheon");
        assert_eq!(intent, TvEpisodeIntent::LatestEpisode);
    }

    #[test]
    fn parse_tv_intent_latest_season_first_episode() {
        let (q, intent) = parse_tv_episode_intent("Pantheon latest season first episode");
        assert_eq!(q, "Pantheon");
        assert_eq!(intent, TvEpisodeIntent::LatestSeasonFirstEpisode);

        let (q, intent) = parse_tv_episode_intent("Pantheon first episode of the latest season");
        assert_eq!(q, "Pantheon");
        assert_eq!(intent, TvEpisodeIntent::LatestSeasonFirstEpisode);
    }

    #[test]
    fn parse_tv_intent_first_episode() {
        let (q, intent) = parse_tv_episode_intent("Pantheon first episode");
        assert_eq!(q, "Pantheon");
        assert_eq!(intent, TvEpisodeIntent::FirstEpisode);
    }

    #[test]
    fn latest_episode_intent_uses_tmdb_last_episode_to_air() {
        let latest = EpisodeRef {
            season: 2,
            episode: 8,
            name: None,
            air_date: None,
        };
        assert_eq!(
            episode_to_search(None, None, TvEpisodeIntent::LatestEpisode, Some(&latest)),
            (2, 8)
        );
    }

    #[test]
    fn latest_season_intent_starts_latest_season() {
        let latest = EpisodeRef {
            season: 2,
            episode: 8,
            name: None,
            air_date: None,
        };
        assert_eq!(
            episode_to_search(
                None,
                None,
                TvEpisodeIntent::LatestSeasonFirstEpisode,
                Some(&latest)
            ),
            (2, 1)
        );
    }

    #[test]
    fn explicit_episode_can_combine_with_latest_season() {
        let latest = EpisodeRef {
            season: 2,
            episode: 8,
            name: None,
            air_date: None,
        };
        assert_eq!(
            episode_to_search(
                None,
                Some(3),
                TvEpisodeIntent::LatestSeasonFirstEpisode,
                Some(&latest)
            ),
            (2, 3)
        );
    }

    #[test]
    fn parse_markers_does_not_eat_codec_tokens() {
        // "1080p" / "h.264" / "H 265" / "5.1" audio MUST NOT be parsed
        // as season/episode markers. None of these contain the SxxExx
        // / NxM / verbal markers.
        let (q, s, e) = parse_episode_markers("The Boys 1080p H.264 5.1 atmos");
        assert!(
            s.is_none(),
            "False match on codec tokens: parsed season={:?}",
            s
        );
        assert!(
            e.is_none(),
            "False match on codec tokens: parsed episode={:?}",
            e
        );
        // Cleaned query may still contain the codec tokens — that's fine,
        // TMDB tolerates extra tokens (though it'll get cleaner search).
        assert!(q.contains("The Boys"));
    }

    #[test]
    fn parse_markers_does_not_eat_4k_marker() {
        // "4K" is not an episode marker. (Won't match anyway because
        // RE_NXM requires digit-x-digit, and `4K` is digit-letter.)
        let (q, s, e) = parse_episode_markers("Dune part 2 4K");
        assert!(
            s.is_none() && e.is_none(),
            "Spurious 4K parse: q={:?} s={:?} e={:?}",
            q,
            s,
            e
        );
    }

    #[test]
    fn parse_markers_zero_clamped_out() {
        // "S00E00" is the pre-air / specials marker on some sites.
        // We clamp 0 out (the API expects 1..=999); user can pass
        // explicit --season 0 if they need it.
        let (_q, s, e) = parse_episode_markers("Show S00E00");
        assert!(s.is_none(), "S00 should clamp to None, got {:?}", s);
        assert!(e.is_none(), "E00 should clamp to None, got {:?}", e);
    }

    #[test]
    fn parse_markers_strips_marker_cleanly() {
        // Cleaned query should have no orphan whitespace.
        let (q, _, _) = parse_episode_markers("The Boys  S05E06  ");
        assert_eq!(q, "The Boys");
    }

    #[test]
    fn parse_markers_marker_in_middle() {
        let (q, s, e) = parse_episode_markers("foo S05E06 bar");
        assert_eq!(q, "foo bar");
        assert_eq!(s, Some(5));
        assert_eq!(e, Some(6));
    }

    #[test]
    fn parse_markers_does_not_match_inside_word() {
        // Word-boundary check: "Bs05e06show" should NOT be parsed.
        let (_q, s, e) = parse_episode_markers("BS05E06show");
        // Note: the `\b` boundary on the left is between 'B' and 'S'
        // which IS a word boundary in Rust regex (transition between
        // alphanumeric chars is NOT a boundary, so this should NOT match).
        // Actually `B` and `S` are both alphanumeric so no boundary —
        // pattern correctly rejects.
        assert!(
            s.is_none() && e.is_none(),
            "Mid-word match: parsed season={:?} episode={:?}",
            s,
            e
        );
    }

    // ============================================================
    // 2026-06-09 TMDB short-query disambiguation tests (TODO entry R)
    // ============================================================

    #[test]
    fn extract_year_strips_parenthesised_year() {
        let (q, y) = extract_year_from_query("Spring (2014)");
        assert_eq!(y, Some(2014));
        assert_eq!(q, "Spring");
    }

    #[test]
    fn extract_year_strips_bare_year() {
        let (q, y) = extract_year_from_query("Spring 2014");
        assert_eq!(y, Some(2014));
        assert_eq!(q, "Spring");
        let (q2, y2) = extract_year_from_query("Dune 2024 Part Two");
        assert_eq!(y2, Some(2024));
        assert_eq!(q2, "Dune Part Two");
    }

    #[test]
    fn extract_year_returns_none_when_absent() {
        let (q, y) = extract_year_from_query("The Boys");
        assert_eq!(y, None);
        assert_eq!(q, "The Boys");
    }

    #[test]
    fn extract_year_rejects_out_of_range() {
        // 1899 too old, 2100 too new → not treated as a year.
        let (_, y1) = extract_year_from_query("Title 1899");
        assert_eq!(y1, None);
        let (_, y2) = extract_year_from_query("Title 2100");
        assert_eq!(y2, None);
    }

    #[test]
    fn extract_year_rejects_digit_run_inside_token() {
        // "S05E06" contains "05" and "06" but NOT a 4-digit year-shaped run
        // at a token boundary. Also "20140" should not be matched as 2014.
        let (q, y) = extract_year_from_query("S05E06");
        assert_eq!(y, None);
        assert_eq!(q, "S05E06");
        let (q2, y2) = extract_year_from_query("Movie 20140");
        assert_eq!(y2, None);
        assert_eq!(q2, "Movie 20140");
    }

    #[test]
    fn extract_year_first_match_wins_for_ambiguous() {
        // "2001 A Space Odyssey 1968" — both look year-shaped. First wins
        // (the canonical title leader is the common case).
        let (q, y) = extract_year_from_query("2001 A Space Odyssey 1968");
        assert_eq!(y, Some(2001));
        assert_eq!(q, "A Space Odyssey 1968");
    }

    fn fake_candidate(title: &str, media_type: &str, year: &str, votes: u64) -> serde_json::Value {
        // TMDB shape: movie uses `title` + `release_date`; tv uses `name` + `first_air_date`.
        if media_type == "tv" {
            serde_json::json!({
                "media_type": "tv",
                "name": title,
                "first_air_date": year,
                "vote_count": votes,
            })
        } else {
            serde_json::json!({
                "media_type": "movie",
                "title": title,
                "release_date": year,
                "vote_count": votes,
            })
        }
    }

    #[test]
    fn score_rewards_title_match_over_noise() {
        // The canonical TODO-entry-R fixture: query `Spring` against
        // [Send Help (2024) — high pop, no token overlap]
        // [Spring (2014) — modest pop, full token overlap].
        // Spring must score higher despite lower votes.
        let q = title_norm("Spring");
        let send_help = fake_candidate("Send Help", "movie", "2024-01-01", 5000);
        let spring = fake_candidate("Spring", "movie", "2014-01-01", 800);
        let s_help = score_tmdb_candidate(&send_help, &q, None);
        let s_spring = score_tmdb_candidate(&spring, &q, None);
        assert!(
            s_spring > s_help,
            "Spring ({}) must beat Send Help ({})",
            s_spring,
            s_help
        );
    }

    #[test]
    fn score_year_match_is_strong_signal() {
        let q = title_norm("Spring");
        // Two candidates both titled "Spring", different years.
        let spring_2014 = fake_candidate("Spring", "movie", "2014-01-01", 500);
        let spring_2020 = fake_candidate("Spring", "movie", "2020-01-01", 500);
        let s14 = score_tmdb_candidate(&spring_2014, &q, Some(2014));
        let s20 = score_tmdb_candidate(&spring_2020, &q, Some(2014));
        assert!(
            s14 > s20,
            "Exact year-match must dominate equal-title alternates: {} vs {}",
            s14,
            s20
        );
    }

    #[test]
    fn score_year_off_by_one_partial_bonus() {
        let q = title_norm("Spring");
        let spring_2014 = fake_candidate("Spring", "movie", "2014-01-01", 100);
        let spring_2015 = fake_candidate("Spring", "movie", "2015-01-01", 100);
        let spring_2020 = fake_candidate("Spring", "movie", "2020-01-01", 100);
        let s14 = score_tmdb_candidate(&spring_2014, &q, Some(2014));
        let s15 = score_tmdb_candidate(&spring_2015, &q, Some(2014));
        let s20 = score_tmdb_candidate(&spring_2020, &q, Some(2014));
        // 2014 (exact) > 2015 (±1) > 2020 (no match).
        assert!(
            s14 > s15 && s15 > s20,
            "year tiers: {} > {} > {}",
            s14,
            s15,
            s20
        );
    }

    #[test]
    fn pick_best_canonical_send_help_vs_spring() {
        // Full TODO-R regression fixture.
        let send_help = fake_candidate("Send Help", "movie", "2024-01-01", 5000);
        let spring_2014 = fake_candidate("Spring", "movie", "2014-01-01", 800);
        let candidates = vec![&send_help, &spring_2014];
        let best = pick_best_tmdb_candidate(&candidates, "Spring", None)
            .expect("Must return a confident pick");
        assert_eq!(best["title"], "Spring", "Must pick Spring, not Send Help");
    }

    #[test]
    fn pick_best_year_overrides_when_query_carries_year() {
        // Two same-titled movies — query "Spring 2014" must pick 2014.
        let spring_2014 = fake_candidate("Spring", "movie", "2014-01-01", 800);
        let spring_2020 = fake_candidate("Spring", "movie", "2020-01-01", 5000);
        let candidates = vec![&spring_2020, &spring_2014]; // pop-ordered as TMDB would
        let best = pick_best_tmdb_candidate(&candidates, "Spring", Some(2014)).unwrap();
        assert_eq!(best["release_date"], "2014-01-01");
    }

    #[test]
    fn pick_best_returns_none_when_floor_not_met() {
        // Query with no title match anywhere and no year hint → None
        // (caller falls back to first-result old behavior).
        let send_help = fake_candidate("Send Help", "movie", "2024-01-01", 5000);
        let other = fake_candidate("Random Movie", "movie", "2020-01-01", 5000);
        let candidates = vec![&send_help, &other];
        let best = pick_best_tmdb_candidate(&candidates, "Spring", None);
        assert!(
            best.is_none(),
            "No-overlap candidates must return None for caller fallback"
        );
    }

    #[test]
    fn pick_best_handles_multi_search_media_types() {
        // Multi-search returns both movie and tv. The best title-match wins
        // regardless of media_type — caller reads media_type off the winner.
        let news_movie = fake_candidate("News at Eleven", "movie", "2024-01-01", 50);
        let the_boys_tv = fake_candidate("The Boys", "tv", "2019-07-26", 15000);
        let candidates = vec![&news_movie, &the_boys_tv];
        let best = pick_best_tmdb_candidate(&candidates, "The Boys", None).unwrap();
        assert_eq!(best["media_type"], "tv");
        assert_eq!(best["name"], "The Boys");
    }
}
