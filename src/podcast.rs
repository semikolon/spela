//! Podcasts (2026-11-04).
//!
//! A show is found in Apple's podcast directory (the iTunes Search API, no key), its RSS
//! feed is read, and an episode plays straight from the publisher's own audio file.
//! No torrent and no transcode are involved: VLC, a Chromecast's Default Media Receiver
//! and a browser `<audio>` element all play MP3/AAC from a URL.
//!
//! The server fetches only two kinds of URL here: Apple's directory, and the feed URL
//! Apple returns for a show id. A client never hands the server a URL to fetch, which is
//! what keeps this from being an SSRF door (`feed_url_allowed` adds a scheme and host
//! check on top). Episode audio is fetched by the PLAYER, never by spela.

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SEARCH_URL: &str = "https://itunes.apple.com/search";
const LOOKUP_URL: &str = "https://itunes.apple.com/lookup";
/// A long-running show's feed can hold thousands of items; 40 MB covers every feed
/// seen so far with room, and refuses a hostile or broken endless body.
const MAX_FEED_BYTES: usize = 40 * 1024 * 1024;
const FEED_TTL: Duration = Duration::from_secs(15 * 60);
const SUMMARY_CHARS: usize = 280;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Show {
    /// Apple's collection id. Stable, and the only handle a client uses for a show.
    pub id: u64,
    pub title: String,
    pub author: String,
    pub artwork: Option<String>,
    pub episode_count: Option<u64>,
    pub genre: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Episode {
    /// The item's `<guid>`, or its audio URL when the feed gives no guid.
    pub guid: String,
    pub title: String,
    /// `YYYY-MM-DD` when the feed's date parses, else the feed's own text.
    pub published: Option<String>,
    pub duration_s: Option<u64>,
    pub audio_url: String,
    pub audio_type: String,
    pub bytes: Option<u64>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Feed {
    pub title: String,
    pub author: String,
    pub artwork: Option<String>,
    /// Newest first.
    pub episodes: Vec<Episode>,
}

static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!(
            "spela/",
            env!("CARGO_PKG_VERSION"),
            " (podcast reader)"
        ))
        .build()
        .expect("reqwest client")
});

/// Show id → feed URL, learned from search results so playing does not need a second
/// directory round-trip.
static FEED_URLS: Lazy<Mutex<HashMap<u64, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static FEEDS: Lazy<Mutex<HashMap<u64, (Instant, Feed)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Search Apple's podcast directory. `country` is a two-letter store code; it decides
/// which national shows rank, so a Swedish show is found from the Swedish store.
pub async fn search(term: &str, country: &str) -> Result<Vec<Show>> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(vec![]);
    }
    let v: serde_json::Value = HTTP
        .get(SEARCH_URL)
        .query(&[
            ("media", "podcast"),
            ("entity", "podcast"),
            ("limit", "25"),
            ("country", country),
            ("term", term),
        ])
        .send()
        .await
        .context("podcast directory unreachable")?
        .error_for_status()?
        .json()
        .await
        .context("podcast directory answered with something other than JSON")?;
    let shows = parse_search(&v);
    if let Ok(mut m) = FEED_URLS.lock() {
        for (id, url) in feed_urls_in(&v) {
            m.insert(id, url);
        }
    }
    Ok(shows)
}

pub fn parse_search(v: &serde_json::Value) -> Vec<Show> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| r["feedUrl"].as_str().is_some_and(|u| !u.is_empty()))
                .filter_map(|r| {
                    Some(Show {
                        id: r["collectionId"].as_u64()?,
                        title: r["collectionName"].as_str()?.to_string(),
                        author: r["artistName"].as_str().unwrap_or("").to_string(),
                        artwork: r["artworkUrl600"]
                            .as_str()
                            .or(r["artworkUrl100"].as_str())
                            .map(String::from),
                        episode_count: r["trackCount"].as_u64(),
                        genre: r["primaryGenreName"].as_str().map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn feed_urls_in(v: &serde_json::Value) -> Vec<(u64, String)> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some((
                        r["collectionId"].as_u64()?,
                        r["feedUrl"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn feed_url_for(id: u64) -> Result<String> {
    if let Some(u) = FEED_URLS.lock().ok().and_then(|m| m.get(&id).cloned()) {
        return Ok(u);
    }
    let v: serde_json::Value = HTTP
        .get(LOOKUP_URL)
        .query(&[("id", id.to_string()), ("entity", "podcast".into())])
        .send()
        .await
        .context("podcast directory unreachable")?
        .error_for_status()?
        .json()
        .await?;
    let url = feed_urls_in(&v)
        .into_iter()
        .find(|(i, _)| *i == id)
        .map(|(_, u)| u)
        .ok_or_else(|| anyhow!("No podcast with id {id} in the directory"))?;
    if let Ok(mut m) = FEED_URLS.lock() {
        m.insert(id, url.clone());
    }
    Ok(url)
}

/// The feed for a show id, from a 15-minute cache or fetched fresh.
pub async fn feed(id: u64) -> Result<Feed> {
    if let Some(f) = FEEDS.lock().ok().and_then(|m| {
        m.get(&id)
            .filter(|(t, _)| t.elapsed() < FEED_TTL)
            .map(|(_, f)| f.clone())
    }) {
        return Ok(f);
    }
    let url = feed_url_for(id).await?;
    if !feed_url_allowed(&url) {
        return Err(anyhow!(
            "The directory gave a feed address spela will not fetch: {url}"
        ));
    }
    let mut resp = HTTP
        .get(&url)
        .send()
        .await
        .with_context(|| format!("feed unreachable: {url}"))?
        .error_for_status()?;
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_FEED_BYTES)
    {
        return Err(anyhow!(
            "feed larger than {} MB",
            MAX_FEED_BYTES / 1024 / 1024
        ));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_FEED_BYTES {
            return Err(anyhow!(
                "feed larger than {} MB",
                MAX_FEED_BYTES / 1024 / 1024
            ));
        }
    }
    let text = String::from_utf8_lossy(&body);
    let f = parse_feed(&text)?;
    if let Ok(mut m) = FEEDS.lock() {
        m.insert(id, (Instant::now(), f.clone()));
    }
    Ok(f)
}

/// One episode of a show, by guid.
pub async fn episode(id: u64, guid: &str) -> Result<(Feed, Episode)> {
    let f = feed(id).await?;
    let ep = f
        .episodes
        .iter()
        .find(|e| e.guid == guid)
        .cloned()
        .ok_or_else(|| anyhow!("That episode is no longer in the feed"))?;
    Ok((f, ep))
}

/// http(s) only, and never a host that names this machine or its LAN. The feed URL
/// comes from Apple's directory rather than from a client, so this is a second fence,
/// not the first: a name can still resolve to a private address.
pub fn feed_url_allowed(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    let h = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if h == "localhost"
        || [".local", ".home", ".lan", ".internal", ".localhost"]
            .iter()
            .any(|s| h.ends_with(s))
    {
        return false;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified())
            }
            std::net::IpAddr::V6(v6) => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80)
            }
        };
    }
    true
}

const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";

fn child_text<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| {
            c.is_element() && c.tag_name().name() == name && c.tag_name().namespace().is_none()
        })
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn itunes_child<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children().find(|c| {
        c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == Some(ITUNES_NS)
    })
}

/// Parse an RSS 2.0 podcast feed. Items without an audio enclosure are skipped: there
/// is nothing to play.
pub fn parse_feed(xml: &str) -> Result<Feed> {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc =
        roxmltree::Document::parse_with_options(xml, opts).context("feed is not valid XML")?;
    let channel = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "channel")
        .ok_or_else(|| anyhow!("feed has no <channel>"))?;
    let title = child_text(channel, "title").unwrap_or("").to_string();
    let author = itunes_child(channel, "author")
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let artwork = itunes_child(channel, "image")
        .and_then(|n| n.attribute("href"))
        .map(String::from)
        .or_else(|| {
            channel
                .children()
                .find(|c| c.is_element() && c.tag_name().name() == "image")
                .and_then(|img| child_text(img, "url"))
                .map(String::from)
        });
    let mut episodes: Vec<(Option<chrono::DateTime<chrono::FixedOffset>>, Episode)> = channel
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "item")
        .filter_map(|item| {
            let enc = item
                .children()
                .find(|c| c.is_element() && c.tag_name().name() == "enclosure")?;
            let audio_url = enc.attribute("url")?.trim().to_string();
            if audio_url.is_empty() {
                return None;
            }
            let audio_type = enc
                .attribute("type")
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| audio_type_from_url(&audio_url).to_string());
            if audio_type.starts_with("video/") {
                return None;
            }
            let date_raw = child_text(item, "pubDate");
            let date = date_raw.and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok());
            let summary_raw = itunes_child(item, "summary")
                .and_then(|n| n.text())
                .or_else(|| child_text(item, "description"))
                .unwrap_or("");
            Some((
                date,
                Episode {
                    guid: child_text(item, "guid")
                        .map(String::from)
                        .unwrap_or_else(|| audio_url.clone()),
                    title: child_text(item, "title")
                        .unwrap_or("Untitled episode")
                        .to_string(),
                    published: date
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .or(date_raw.map(String::from)),
                    duration_s: itunes_child(item, "duration")
                        .and_then(|n| n.text())
                        .and_then(parse_duration),
                    bytes: enc
                        .attribute("length")
                        .and_then(|l| l.trim().parse().ok())
                        .filter(|&n: &u64| n > 0),
                    audio_url,
                    audio_type,
                    summary: plain_summary(summary_raw),
                },
            ))
        })
        .collect();
    // Newest first. A stable sort keeps the feed's own order among undated items.
    episodes.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(Feed {
        title,
        author,
        artwork,
        episodes: episodes.into_iter().map(|(_, e)| e).collect(),
    })
}

/// `itunes:duration` comes as seconds (`3742`), `MM:SS` or `HH:MM:SS`, sometimes with
/// a fraction.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0f64;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    (total > 0.0).then_some(total.round() as u64)
}

pub fn audio_type_from_url(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if path.ends_with(".m4a") || path.ends_with(".m4b") || path.ends_with(".mp4") {
        "audio/mp4"
    } else if path.ends_with(".aac") {
        "audio/aac"
    } else if path.ends_with(".ogg") || path.ends_with(".oga") || path.ends_with(".opus") {
        "audio/ogg"
    } else if path.ends_with(".flac") {
        "audio/flac"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
}

static TAG: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(?s)<[^>]*>").unwrap());
static SPACE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\s+").unwrap());

/// Show notes come as HTML; the list shows a short plain-text line.
pub fn plain_summary(html: &str) -> String {
    let no_tags = TAG.replace_all(html, " ");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#8217;", "\u{2019}");
    let flat = SPACE.replace_all(decoded.trim(), " ").to_string();
    if flat.chars().count() <= SUMMARY_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(SUMMARY_CHARS).collect();
    let cut = cut
        .rsplit_once(' ')
        .map(|(a, _)| a.to_string())
        .unwrap_or(cut);
    format!("{cut}…")
}

/// A VLC playlist for one episode. VLC fetches the audio itself; `start-time` resumes.
pub fn vlc_playlist(show: &str, ep: &Episode, start_s: Option<u64>) -> String {
    let start = start_s
        .filter(|&s| s > 0)
        .map(|s| format!("#EXTVLCOPT:start-time={s}\n"))
        .unwrap_or_default();
    format!(
        "#EXTM3U\n#EXTINF:{},{} — {}\n{}#EXTVLCOPT:network-caching=3000\n{}\n",
        ep.duration_s.map(|d| d as i64).unwrap_or(-1),
        one_line(show),
        one_line(&ep.title),
        start,
        ep.audio_url
    )
}

pub(crate) fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copied verbatim from a real feed (Lex Fridman Podcast, served by Blubrry,
    // fetched 2026-11-04) and cut after its first two items.
    const FEED: &str = include_str!("../tests/fixtures/podcast/feed_two_items.xml");

    #[test]
    fn parses_a_real_feed_newest_first() {
        let f = parse_feed(FEED).unwrap();
        assert!(!f.title.is_empty());
        assert_eq!(f.episodes.len(), 2);
        assert!(f.episodes[0].published >= f.episodes[1].published);
        for e in &f.episodes {
            assert!(e.audio_url.starts_with("http"));
            assert!(e.audio_type.starts_with("audio/"));
            assert!(e.duration_s.unwrap_or(0) > 60);
            assert!(!e.summary.contains('<'));
        }
    }

    /// Real shows through the real directory and their real feeds. Network, so ignored
    /// by default: `cargo test podcast -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_directory_and_feeds() {
        for (q, country) in [
            ("lex fridman", "us"),
            ("sanningens mammor", "se"),
            ("making sense sam harris", "us"),
        ] {
            let shows = search(q, country).await.unwrap();
            assert!(!shows.is_empty(), "no show for {q}");
            let f = feed(shows[0].id).await.unwrap();
            assert!(!f.episodes.is_empty(), "no episodes for {q}");
            assert!(f.episodes.iter().all(|e| e.audio_url.starts_with("http")));
        }
    }

    #[test]
    fn durations_in_every_shape() {
        assert_eq!(parse_duration("3742"), Some(3742));
        assert_eq!(parse_duration("62:22"), Some(3742));
        assert_eq!(parse_duration("1:02:22"), Some(3742));
        assert_eq!(parse_duration("01:02:22.4"), Some(3742));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn items_without_audio_are_skipped_and_guidless_items_use_the_url() {
        let xml = r#"<rss><channel><title>T</title>
            <item><title>No audio</title></item>
            <item><title>Video</title><enclosure url="https://x.example/v.mp4" type="video/mp4"/></item>
            <item><title>Audio</title><enclosure url="https://x.example/a.mp3?u=1"/></item>
            </channel></rss>"#;
        let f = parse_feed(xml).unwrap();
        assert_eq!(f.episodes.len(), 1);
        assert_eq!(f.episodes[0].guid, "https://x.example/a.mp3?u=1");
        assert_eq!(f.episodes[0].audio_type, "audio/mpeg");
    }

    #[test]
    fn feed_urls_that_name_this_network_are_refused() {
        assert!(feed_url_allowed("https://feeds.example.com/show.rss"));
        assert!(!feed_url_allowed("file:///etc/passwd"));
        assert!(!feed_url_allowed("http://localhost:7890/status"));
        assert!(!feed_url_allowed("http://127.0.0.1/x"));
        assert!(!feed_url_allowed("http://192.168.1.1/x"));
        assert!(!feed_url_allowed("http://10.0.0.2/x"));
        assert!(!feed_url_allowed("http://server.home/x"));
        assert!(!feed_url_allowed("http://[::1]/x"));
    }

    #[test]
    fn summaries_are_plain_and_short() {
        let s = plain_summary("<p>Hello&nbsp;<b>world</b> &amp; friends</p>");
        assert_eq!(s, "Hello world & friends");
        let long = "word ".repeat(200);
        let cut = plain_summary(&long);
        assert!(cut.chars().count() <= SUMMARY_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn directory_results_without_a_feed_are_dropped() {
        let v = serde_json::json!({"results":[
            {"collectionId":1,"collectionName":"A","artistName":"x","feedUrl":"https://a.example/f"},
            {"collectionId":2,"collectionName":"B","artistName":"y"}
        ]});
        let s = parse_search(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, 1);
    }

    #[test]
    fn vlc_playlist_resumes_and_keeps_one_line_titles() {
        let ep = Episode {
            guid: "g".into(),
            title: "Line\nbreak".into(),
            published: None,
            duration_s: Some(100),
            audio_url: "https://a.example/e.mp3".into(),
            audio_type: "audio/mpeg".into(),
            bytes: None,
            summary: String::new(),
        };
        let m = vlc_playlist("Show", &ep, Some(42));
        assert!(m.contains("#EXTVLCOPT:start-time=42"));
        assert!(m.contains("Line break"));
        assert!(m.trim_end().ends_with("https://a.example/e.mp3"));
    }
}
