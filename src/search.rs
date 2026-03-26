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

        // Step 3: Torrentio lookup
        let results = self.torrentio_streams(&imdb_id, Some(s), Some(e)).await?;
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

        let results = self.torrentio_streams(&imdb_id, None, None).await?;
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

    async fn torrentio_streams(&self, imdb_id: &str, season: Option<u32>, episode: Option<u32>) -> Result<Vec<TorrentResult>> {
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
        let mut results: Vec<TorrentResult> = streams.iter().map(|s| {
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

        // Smart ranking (3 tiers):
        // 1. Single-file > season pack (webtorrent -s is unreliable for packs)
        // 2. H.264 > HEVC/x265, BUT only if H.264 has enough seeds (≥5).
        //    A well-seeded HEVC beats a dead H.264 — NVENC handles the transcode.
        // 3. More seeds > fewer seeds
        const MIN_SEEDS_FOR_CODEC_PREF: u32 = 5;
        results.sort_by(|a, b| {
            let a_single = a.file_index.map_or(true, |i| i == 0);
            let b_single = b.file_index.map_or(true, |i| i == 0);
            if a_single != b_single {
                return if a_single { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
            }

            let a_hevc = is_hevc_from_title(&a.title);
            let b_hevc = is_hevc_from_title(&b.title);
            // Only apply codec preference when the preferred result has viable seeds
            if a_hevc != b_hevc {
                let preferred = if a_hevc { b } else { a }; // the H.264 one
                if preferred.seeds >= MIN_SEEDS_FOR_CODEC_PREF {
                    return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                }
            }

            b.seeds.cmp(&a.seeds)
        });

        // Assign IDs after sorting
        for (i, r) in results.iter_mut().enumerate() {
            r.id = i + 1;
        }

        Ok(results.into_iter().take(8).collect())
    }
}

/// Detect HEVC/x265 from torrent filename — these need NVENC re-encoding for Chromecast.
fn is_hevc_from_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.contains("x265") || lower.contains("h265") || lower.contains("h.265")
        || lower.contains("hevc") || lower.contains("10bit") || lower.contains("10-bit")
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
        results.sort_by(|a, b| {
            let a_single = a.file_index.map_or(true, |i| i == 0);
            let b_single = b.file_index.map_or(true, |i| i == 0);
            if a_single != b_single {
                return if a_single { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
            }
            b.seeds.cmp(&a.seeds)
        });
        assert_eq!(results[0].seeds, 50); // single file wins despite fewer seeds
    }

    #[test]
    fn test_ranking_h264_over_hevc_with_enough_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)),   // HEVC, well-seeded
            make_result(2, "Movie.x264.mkv", 20, Some(0)),    // H.264, decent seeds
        ];
        const MIN_SEEDS: u32 = 5;
        results.sort_by(|a, b| {
            let a_hevc = is_hevc_from_title(&a.title);
            let b_hevc = is_hevc_from_title(&b.title);
            if a_hevc != b_hevc {
                let preferred = if a_hevc { b } else { a };
                if preferred.seeds >= MIN_SEEDS {
                    return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                }
            }
            b.seeds.cmp(&a.seeds)
        });
        assert_eq!(results[0].title, "Movie.x264.mkv"); // H.264 wins (20 >= 5 threshold)
    }

    #[test]
    fn test_ranking_h264_at_exact_threshold() {
        // Exactly 5 seeds = threshold met, H.264 should win
        let mut results = vec![
            make_result(1, "Movie.x265.mkv", 100, Some(0)),
            make_result(2, "Movie.x264.mkv", 5, Some(0)),  // exactly at threshold
        ];
        const MIN_SEEDS: u32 = 5;
        results.sort_by(|a, b| {
            let a_hevc = is_hevc_from_title(&a.title);
            let b_hevc = is_hevc_from_title(&b.title);
            if a_hevc != b_hevc {
                let preferred = if a_hevc { b } else { a };
                if preferred.seeds >= MIN_SEEDS {
                    return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                }
            }
            b.seeds.cmp(&a.seeds)
        });
        assert_eq!(results[0].title, "Movie.x264.mkv");
    }

    #[test]
    fn test_ranking_both_h264_sorts_by_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x264.FLEET.mkv", 10, Some(0)),
            make_result(2, "Movie.x264.YTS.mp4", 50, Some(0)),
        ];
        results.sort_by(|a, b| {
            let a_hevc = is_hevc_from_title(&a.title);
            let b_hevc = is_hevc_from_title(&b.title);
            if a_hevc != b_hevc {
                let preferred = if a_hevc { b } else { a };
                if preferred.seeds >= 5 {
                    return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                }
            }
            b.seeds.cmp(&a.seeds)
        });
        assert_eq!(results[0].seeds, 50); // more seeds wins within same codec tier
    }

    #[test]
    fn test_ranking_both_hevc_sorts_by_seeds() {
        let mut results = vec![
            make_result(1, "Movie.x265.10Bit.mkv", 200, Some(0)),
            make_result(2, "Movie.HEVC.mkv", 50, Some(0)),
        ];
        results.sort_by(|a, b| b.seeds.cmp(&a.seeds));
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
        const MIN_SEEDS: u32 = 5;
        results.sort_by(|a, b| {
            let a_hevc = is_hevc_from_title(&a.title);
            let b_hevc = is_hevc_from_title(&b.title);
            if a_hevc != b_hevc {
                let preferred = if a_hevc { b } else { a };
                if preferred.seeds >= MIN_SEEDS {
                    return if a_hevc { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                }
            }
            b.seeds.cmp(&a.seeds)
        });
        assert_eq!(results[0].title, "Movie.x265.mkv"); // HEVC wins (H.264 only 2 seeds < 5)
    }
}
