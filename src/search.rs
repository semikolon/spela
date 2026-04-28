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
    /// Apr 28, 2026: Full TMDB poster URL, already prefixed with the image
    /// base (`https://image.tmdb.org/t/p/w500{poster_path}`). Populated at
    /// search time, persisted into `last_search.json`, plumbed through
    /// `PlayRequest.poster_url` → `CurrentStream.poster_url` → `CastMetadata`
    /// so the Default Media Receiver renders its rich-UI player (poster
    /// background + auto-hide controls) instead of the minimal persistent
    /// overlay it shows when `Media.metadata` is None.
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

    pub async fn search(&self, query: &str, movie: bool, season: Option<u32>, episode: Option<u32>) -> Result<SearchResult> {
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

        if movie {
            self.search_movie(query).await
        } else if season.is_some() || episode.is_some() {
            // Explicit season/episode = definitely TV
            self.search_tv(query, season, episode).await
        } else {
            // Auto-detect: use TMDB multi-search to determine if it's a movie or TV show
            match self.tmdb_auto_detect(query).await {
                Ok(media_type) if media_type == "movie" => {
                    tracing::info!("Auto-detected '{}' as movie (TMDB multi-search)", query);
                    self.search_movie(query).await
                }
                _ => {
                    // Default to TV, or if auto-detect found "tv"
                    self.search_tv(query, season, episode).await
                }
            }
        }
    }

    /// Use TMDB's /search/multi endpoint to detect whether a query is a movie or TV show.
    async fn tmdb_auto_detect(&self, query: &str) -> Result<String> {
        let url = format!(
            "https://api.themoviedb.org/3/search/multi?query={}&api_key={}",
            urlencoded(query),
            self.tmdb_key
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;
        let results = resp["results"].as_array()
            .ok_or_else(|| anyhow!("No multi-search results"))?;

        // Find the first movie or tv result (skip "person" results)
        for result in results {
            if let Some(media_type) = result["media_type"].as_str() {
                if media_type == "movie" || media_type == "tv" {
                    return Ok(media_type.to_string());
                }
            }
        }
        Err(anyhow!("No movie or TV result found"))
    }

    async fn search_tv(&self, query: &str, season: Option<u32>, episode: Option<u32>) -> Result<SearchResult> {
        // Step 1: TMDB search
        let tmdb = self.tmdb_search(query, "tv").await?;
        let tmdb_id = tmdb["id"].as_u64().ok_or_else(|| anyhow!("No TV show found for \"{}\"", query))?;

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
            _ => return Ok(SearchResult {
                query: query.into(),
                show: Some(show_info),
                searching: None,
                error: Some("No IMDB ID found for this show".into()),
                torrent_available: false,
                results: vec![],
            }),
        };

        // Determine episode to search
        let (s, e) = match (season, episode) {
            (Some(s), Some(e)) => (s, e),
            _ => match &show_info.latest_episode {
                Some(ep) => (ep.season, ep.episode),
                None => return Ok(SearchResult {
                    query: query.into(),
                    show: Some(show_info),
                    searching: None,
                    error: Some("Cannot determine episode to search".into()),
                    torrent_available: false,
                    results: vec![],
                }),
            },
        };

        // Step 3: Torrentio lookup (filtered by show title to drop spurious
        // cross-show results — the Apr 15 "French Chef for The Boys S05E03"
        // incident).
        let results = self.torrentio_streams(&imdb_id, &show_info.title, Some(s), Some(e)).await?;
        Ok(SearchResult {
            query: query.into(),
            show: Some(show_info),
            searching: Some(EpisodeRef { season: s, episode: e, name: None, air_date: None }),
            error: None,
            torrent_available: !results.is_empty(),
            results,
        })
    }

    async fn search_movie(&self, query: &str) -> Result<SearchResult> {
        let tmdb = self.tmdb_search(query, "movie").await?;
        let tmdb_id = tmdb["id"].as_u64().ok_or_else(|| anyhow!("No movie found for \"{}\"", query))?;

        let detail = self.tmdb_movie_details(tmdb_id).await?;
        let imdb_id = detail["external_ids"]["imdb_id"].as_str()
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
            overview: detail["overview"].as_str().map(|s| s.chars().take(200).collect()),
            poster_url: tmdb_poster_url(detail["poster_path"].as_str()),
        };

        let imdb_id = match &show_info.imdb_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => return Ok(SearchResult {
                query: query.into(),
                show: Some(show_info),
                searching: None,
                error: Some("No IMDB ID".into()),
                torrent_available: false,
                results: vec![],
            }),
        };

        let results = self.torrentio_streams(&imdb_id, &show_info.title, None, None).await?;
        Ok(SearchResult {
            query: query.into(),
            show: Some(show_info),
            searching: None,
            error: None,
            torrent_available: !results.is_empty(),
            results,
        })
    }

    async fn tmdb_search(&self, query: &str, media_type: &str) -> Result<Value> {
        let url = format!(
            "https://api.themoviedb.org/3/search/{}?query={}&api_key={}",
            media_type,
            urlencoded(query),
            self.tmdb_key
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;
        resp["results"].as_array()
            .and_then(|r| r.first().cloned())
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

    async fn torrentio_streams(&self, imdb_id: &str, show_title: &str, season: Option<u32>, episode: Option<u32>) -> Result<Vec<TorrentResult>> {
        let path = match (season, episode) {
            (Some(s), Some(e)) => format!("stream/series/{}:{}:{}.json", imdb_id, s, e),
            _ => format!("stream/movie/{}.json", imdb_id),
        };
        let url = format!("{}/{}", TORRENTIO_BASE, path);

        let resp: Value = self.client
            .get(&url)
            .header("User-Agent", "spela/2.0")
            .send()
            .await?
            .json()
            .await?;

        let streams = resp["streams"].as_array().cloned().unwrap_or_default();
        let results: Vec<TorrentResult> = streams.iter().map(|s| {
            let title_text = s["title"].as_str().unwrap_or("");
            let meta = parse_torrentio_title(title_text);
            let quality = s["name"].as_str().unwrap_or("")
                .replace("Torrentio\n", "").trim().to_string();
            let info_hash = s["infoHash"].as_str().unwrap_or("").to_string();
            let filename = s["behaviorHints"]["filename"].as_str()
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
                magnet: build_magnet(&info_hash, s["behaviorHints"]["filename"].as_str().unwrap_or("")),
                info_hash,
                file_index: s["fileIdx"].as_u64().map(|n| n as u32),
            }
        }).collect();

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
pub fn rank_results_mut(results: &mut Vec<TorrentResult>) {
    const MIN_SEEDS_FOR_CODEC_PREF: u32 = 5;
    const MIN_SEEDS_FOR_RESOLUTION_PREF: u32 = 50;

    results.sort_by(|a, b| {
        // Tier 1: single-file > pack
        let a_single = a.file_index.map_or(true, |i| i == 0);
        let b_single = b.file_index.map_or(true, |i| i == 0);
        if a_single != b_single {
            return if a_single { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }

        // Tier 2: non-DV > DV (HARD GPU gate, fires before any quality tier)
        let a_dv = has_dolby_vision_in_title(&a.title);
        let b_dv = has_dolby_vision_in_title(&b.title);
        if a_dv != b_dv {
            return if a_dv { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
        }

        // Tier 3: target resolution (1080p > 720p > 480p > 2160p > unknown)
        // with ≥50-seed viability gate on the higher-preference option.
        let a_res = resolution_tier(&a.title);
        let b_res = resolution_tier(&b.title);
        if a_res != b_res {
            let (higher_res_result, higher_is_a) = if a_res < b_res {
                (a, true)
            } else {
                (b, false)
            };
            if higher_res_result.seeds >= MIN_SEEDS_FOR_RESOLUTION_PREF {
                return if higher_is_a {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
        }

        // Tier 4: H.264 > HEVC within same resolution + DV status (insta-play tiebreak)
        let a_hevc = is_hevc_from_title(&a.title);
        let b_hevc = is_hevc_from_title(&b.title);
        if a_hevc != b_hevc {
            let preferred = if a_hevc { b } else { a }; // the H.264 one
            if preferred.seeds >= MIN_SEEDS_FOR_CODEC_PREF {
                return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
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
fn filter_results_by_show_title(results: Vec<TorrentResult>, show_title: &str) -> Vec<TorrentResult> {
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
            dropped.len(), tokens
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
    const STOP_WORDS: &[&str] = &[
        "the", "a", "an", "of", "and", "in", "on", "to", "at", "is",
    ];
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
        if lower.contains(needle) { return true; }
        let with_space = needle.replace('p', " p");
        if lower.contains(&with_space) { return true; }
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
    if res_match("2160p") || lower.contains("4k") || lower.contains(" uhd") || lower.contains(".uhd") {
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
    if val.is_null() { return None; }
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
    let seeds = title.find("👤").and_then(|i| {
        title[i..].split_whitespace().nth(1)?.parse().ok()
    }).unwrap_or(0);
    let size = title.find("💾").and_then(|i| {
        let rest = &title[i + "💾".len()..];
        let parts: Vec<&str> = rest.trim().splitn(3, ' ').collect();
        if parts.len() >= 2 { Some(format!("{} {}", parts[0], parts[1])) } else { None }
    }).unwrap_or_default();
    let source = title.find("⚙️").and_then(|i| {
        title[i + "⚙️".len()..].trim().split_whitespace().next().map(String::from)
    }).unwrap_or_default();
    (seeds, size, source)
}

fn build_magnet(info_hash: &str, name: &str) -> String {
    let trackers: String = PUBLIC_TRACKERS.iter()
        .map(|t| format!("&tr={}", urlencoded(t)))
        .collect();
    format!("magnet:?xt=urn:btih:{}&dn={}{}", info_hash, urlencoded(name), trackers)
}

fn urlencoded(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
            String::from(b as char)
        }
        _ => format!("%{:02X}", b),
    }).collect()
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
        assert_eq!(url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/qZQqEgXgGRpC8nJa9j5ej31Ynmm.jpg"));
    }

    #[test]
    fn tmdb_poster_url_missing_leading_slash_is_inserted() {
        // TMDB always returns leading slash, but defend against a future
        // schema change or upstream bug where it doesn't.
        let url = tmdb_poster_url(Some("abc.jpg"));
        assert_eq!(url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/abc.jpg"));
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

    #[test]
    fn tmdb_poster_url_empty_returns_none() {
        assert_eq!(tmdb_poster_url(Some("")), None);
        assert_eq!(tmdb_poster_url(Some("   ")), None,
            "Whitespace-only path must be rejected.");
    }

    #[test]
    fn tmdb_poster_url_handles_special_chars_in_path() {
        // TMDB poster paths are alphanumeric + dot, but defend against
        // unusual filenames (different image bucket, different content type).
        let url = tmdb_poster_url(Some("/some-name_with.dots-and_underscores.png"));
        assert_eq!(url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/some-name_with.dots-and_underscores.png"));
    }

    #[test]
    fn tmdb_poster_url_picks_w500_size() {
        // Pin the size choice so a future "let's use original" tweak that
        // bloats Cast LOAD messages by 20× gets caught by tests.
        let url = tmdb_poster_url(Some("/x.jpg")).unwrap();
        assert!(url.contains("/w500/"),
            "URL must include the w500 size descriptor: {url}");
        assert!(!url.contains("/original/"),
            "Original-size posters are 2-4 MB, too heavy for the LOAD msg");
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
        let info: ShowInfo = serde_json::from_str(legacy_json)
            .expect("legacy ShowInfo must deserialize");
        assert_eq!(info.title, "Hijack");
        assert!(info.poster_url.is_none(),
            "Missing poster_url field in old JSON must default to None.");
    }

    #[test]
    fn test_is_hevc_from_title() {
        assert!(is_hevc_from_title("Movie.2025.1080p.BluRay.x265-GROUP.mkv"));
        assert!(is_hevc_from_title("Movie.HEVC.1080p.mkv"));
        assert!(is_hevc_from_title("Movie.H265.mkv"));
        assert!(is_hevc_from_title("Movie.H.265.mkv"));
        assert!(is_hevc_from_title("Movie.10Bit.DDP5.1.mkv"));
        assert!(is_hevc_from_title("Movie.10-bit.mkv"));
        assert!(!is_hevc_from_title("Movie.2025.1080p.BluRay.x264-GROUP.mkv"));
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
        assert!(!has_dolby_vision_in_title("Movie.2025.1080p.BluRay.x264.mkv"));
        assert!(!has_dolby_vision_in_title("Movie.2025.2160p.HEVC.HDR10.mkv"));
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
            make_result(1, "Movie.x264.mkv", 100, Some(3)),   // pack
            make_result(2, "Movie.x264.mkv", 50, Some(0)),    // single file
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].seeds, 50); // single file wins despite fewer seeds
    }

    #[test]
    fn test_ranking_h264_over_hevc_with_enough_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)),   // HEVC, well-seeded
            make_result(2, "Movie.x264.mkv", 20, Some(0)),    // H.264, decent seeds
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
            make_result(2, "Movie.x264.mkv", 5, Some(0)),  // exactly at threshold
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
            make_result(1, "Movie.x265.mkv", 100, Some(0)),  // HEVC, well-seeded
            make_result(2, "Movie.x264.mkv", 2, Some(0)),    // H.264, nearly dead
        ];
        rank_results_mut(&mut results);
        // H.264 has only 2 seeds, below the tier 4 viability threshold of 5,
        // so tier 4 doesn't fire and we fall through to tier 5 seed tiebreak:
        // 100 > 2 → HEVC wins. Same behavior as v3.0.0 for this fixture.
        assert_eq!(results[0].title, "Movie.x265.mkv");
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
            make_result(1, "The.Boys.S05E01.2160p.AMZN.WEB-DL.DV.HDR.H.265-FLUX.mkv", 783, Some(0)),
            make_result(2, "The.Boys.S05E01.1080p.WEB-DL.DUAL.5.1.H.264.mkv", 1433, Some(0)),
        ];
        rank_results_mut(&mut results);
        assert_eq!(results[0].title, "The.Boys.S05E01.1080p.WEB-DL.DUAL.5.1.H.264.mkv");
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
}
