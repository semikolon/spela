//! Audiobooks (2026-11-04).
//!
//! spela's film and series search goes through Torrentio, which answers only lookups by
//! IMDb or Kitsu id, so it cannot find a book. Audiobooks are searched by title instead,
//! in three places, and played through the same embedded torrent engine:
//!
//! - **The Pirate Bay's API** (`apibay.org`, category 102 = audiobooks): live seeder counts.
//! - **Knaben** (`api.knaben.org`): one index over the Pirate Bay, 1337x, Rutracker and
//!   other sites, so a book listed on one of those is found without scraping each.
//! - **AudioBook Bay** (`audiobookbay.lu`): the largest audiobook-only tracker. Its pages
//!   carry the info hash and size but no seeder count.
//!
//! Measured on 2026-11-04 from a home connection: 1337x, TorrentGalaxy and
//! MagnetDL did not connect, and KickassTorrents and EXT answer scripts with a Cloudflare
//! 403, so they are reached through Knaben or not at all. A source that fails is reported
//! by name in the answer rather than hidden.
//!
//! A book's torrent usually holds one file per chapter. The player gets every audio file
//! in natural name order (so "Part 2" comes before "Part 10"), as a VLC playlist or as a
//! list the web remote walks through.

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const APIBAY: &str = "https://apibay.org/q.php";
const KNABEN: &str = "https://api.knaben.org/v1";
pub const AUDIOBOOK_BAY: &str = "https://audiobookbay.lu";
/// Book pages fetched per search to read their info hash. Each is one request, and the
/// best matches come first, so a handful is enough.
const ABB_DETAIL_PAGES: usize = 5;
const MAX_RESULTS: usize = 30;

/// Cloudflare in front of these sites refuses obvious script clients, so the client
/// identifies as a browser. Nothing is logged in to and nothing is posted.
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15")
        .build()
        .expect("reqwest client")
});

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Book {
    /// Lowercase 40-character hex info hash. The only handle a client uses.
    pub hash: String,
    pub name: String,
    /// `None` when no source reports it (AudioBook Bay alone).
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub bytes: Option<u64>,
    pub files: Option<u32>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SearchOutcome {
    pub books: Vec<Book>,
    /// Sources that did not answer, by name.
    pub failed: Vec<String>,
}

pub async fn search(q: &str) -> SearchOutcome {
    let q = q.trim();
    if q.is_empty() {
        return SearchOutcome::default();
    }
    let (a, k, b) = tokio::join!(search_apibay(q), search_knaben(q), search_abb(q));
    let mut out = SearchOutcome::default();
    let mut all = Vec::new();
    for (name, r) in [("The Pirate Bay", a), ("Knaben", k), ("AudioBook Bay", b)] {
        match r {
            Ok(v) => all.extend(v),
            Err(e) => {
                tracing::warn!("audiobook search: {} failed: {:#}", name, e);
                out.failed.push(name.to_string());
            }
        }
    }
    out.books = merge_and_rank(all, q);
    out
}

/// One row per info hash, the highest seeder count any source reports, and only rows
/// whose name carries every significant word of the query.
pub fn merge_and_rank(all: Vec<Book>, q: &str) -> Vec<Book> {
    let mut by_hash: HashMap<String, Book> = HashMap::new();
    for b in all {
        if !matches_query(&b.name, q) {
            continue;
        }
        by_hash
            .entry(b.hash.clone())
            .and_modify(|e| {
                e.seeders = e.seeders.max(b.seeders);
                e.leechers = e.leechers.max(b.leechers);
                e.bytes = e.bytes.or(b.bytes);
                e.files = e.files.or(b.files);
                for s in &b.sources {
                    if !e.sources.contains(s) {
                        e.sources.push(s.clone());
                    }
                }
            })
            .or_insert(b);
    }
    let mut v: Vec<Book> = by_hash.into_values().collect();
    // Live swarms first by size of swarm; then books whose swarm no source reports
    // (AudioBook Bay alone), which are often alive; a reported zero last.
    v.sort_by(|a, b| {
        let rank = |x: &Book| match x.seeders {
            Some(n) if n > 0 => (0u8, u32::MAX - n),
            None => (1, 0),
            Some(_) => (2, 0),
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    v.truncate(MAX_RESULTS);
    v
}

const STOP: &[&str] = &[
    "the",
    "and",
    "a",
    "an",
    "of",
    "by",
    "to",
    "in",
    "on",
    "for",
    "audiobook",
];

static POSSESSIVE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"['’]s\b").unwrap());

fn words(s: &str) -> Vec<String> {
    POSSESSIVE
        .replace_all(&s.to_lowercase(), "")
        .replace(['\'', '’'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

pub fn matches_query(name: &str, q: &str) -> bool {
    let have = words(name);
    words(q)
        .iter()
        .filter(|w| !STOP.contains(&w.as_str()))
        .all(|w| have.iter().any(|h| h == w))
}

pub fn valid_hash(h: &str) -> Option<String> {
    let h = h.trim().to_ascii_lowercase();
    (h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn magnet(hash: &str, name: &str) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{hash}&dn={}", pct(name));
    for t in crate::search::PUBLIC_TRACKERS {
        m.push_str("&tr=");
        m.push_str(&pct(t));
    }
    m
}

async fn search_apibay(q: &str) -> Result<Vec<Book>> {
    let v: serde_json::Value = HTTP
        .get(APIBAY)
        .query(&[("q", q), ("cat", "102")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_apibay(&v))
}

pub fn parse_apibay(v: &serde_json::Value) -> Vec<Book> {
    let num = |x: &serde_json::Value| {
        x.as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or(x.as_u64())
    };
    v.as_array()
        .map(|a| {
            a.iter()
                // The API answers "nothing found" with one row whose id is "0".
                .filter(|r| r["id"].as_str() != Some("0"))
                .filter_map(|r| {
                    Some(Book {
                        hash: valid_hash(r["info_hash"].as_str()?)?,
                        name: r["name"].as_str()?.to_string(),
                        seeders: num(&r["seeders"]).map(|n| n as u32),
                        leechers: num(&r["leechers"]).map(|n| n as u32),
                        bytes: num(&r["size"]).filter(|&n| n > 0),
                        files: num(&r["num_files"]).map(|n| n as u32).filter(|&n| n > 0),
                        sources: vec!["The Pirate Bay".into()],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn search_knaben(q: &str) -> Result<Vec<Book>> {
    let body =
        serde_json::json!({"query": q, "search_field": "title", "size": 50, "hide_unsafe": true});
    let v: serde_json::Value = HTTP
        .post(KNABEN)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_knaben(&v))
}

pub fn parse_knaben(v: &serde_json::Value) -> Vec<Book> {
    v["hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|h| {
                    h["category"]
                        .as_str()
                        .is_some_and(|c| c.to_ascii_lowercase().contains("audiobook"))
                })
                .filter_map(|h| {
                    let site = h["tracker"]
                        .as_str()
                        .or(h["cachedOrigin"].as_str())
                        .unwrap_or("Knaben")
                        .trim_end_matches(" (proxy)")
                        .to_string();
                    Some(Book {
                        hash: valid_hash(h["hash"].as_str()?)?,
                        name: h["title"].as_str()?.to_string(),
                        seeders: h["seeders"].as_u64().map(|n| n as u32),
                        leechers: h["peers"].as_u64().map(|n| n as u32),
                        bytes: h["bytes"].as_u64().filter(|&n| n > 0),
                        files: None,
                        sources: vec![site],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

static ABB_ROW: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r#"class="postTitle"><h2><a href="(/abss/[^"]+)"[^>]*>([^<]+)</a>"#).unwrap()
});
static ABB_HASH: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)Info Hash:</td>\s*<td>([0-9a-f]{40})</td>").unwrap());
static ABB_SIZE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)Combined File Size:</td>\s*<td>([^<]+)</td>").unwrap());

pub fn html_text(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#8217;", "\u{2019}")
        .replace("&#039;", "'")
        .replace("&quot;", "\"")
        .trim()
        .to_string()
}

/// AudioBook Bay's search page: `(book page path, title)` per result, in its order.
pub fn parse_abb_search(html: &str) -> Vec<(String, String)> {
    ABB_ROW
        .captures_iter(html)
        .map(|c| (c[1].to_string(), html_text(&c[2])))
        .collect()
}

/// A book page's info hash and combined size ("332.67 MBs").
pub fn parse_abb_detail(html: &str) -> Option<(String, Option<u64>)> {
    let hash = valid_hash(&ABB_HASH.captures(html)?[1])?;
    let bytes = ABB_SIZE.captures(html).and_then(|c| parse_size(&c[1]));
    Some((hash, bytes))
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit): (String, String) = s.chars().partition(|c| c.is_ascii_digit() || *c == '.');
    let n: f64 = num.parse().ok()?;
    let mult = if unit.contains('t') {
        1e12
    } else if unit.contains('g') {
        1e9
    } else if unit.contains('m') {
        1e6
    } else if unit.contains('k') {
        1e3
    } else {
        1.0
    };
    Some((n * mult) as u64)
}

async fn search_abb(q: &str) -> Result<Vec<Book>> {
    let html = HTTP
        .get(format!("{AUDIOBOOK_BAY}/"))
        .query(&[("s", q)])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let rows: Vec<(String, String)> = parse_abb_search(&html)
        .into_iter()
        .filter(|(_, t)| matches_query(t, q))
        .take(ABB_DETAIL_PAGES)
        .collect();
    let handles: Vec<_> = rows
        .iter()
        .map(|(path, _)| {
            let url = format!("{AUDIOBOOK_BAY}{path}");
            tokio::spawn(async move { HTTP.get(url).send().await.ok()?.text().await.ok() })
        })
        .collect();
    let mut pages = Vec::with_capacity(handles.len());
    for h in handles {
        pages.push(h.await.ok().flatten());
    }
    Ok(rows
        .into_iter()
        .zip(pages)
        .filter_map(|((_, title), page)| {
            let (hash, bytes) = parse_abb_detail(&page?)?;
            Some(Book {
                hash,
                name: title,
                seeders: None,
                leechers: None,
                bytes,
                files: None,
                sources: vec!["AudioBook Bay".into()],
            })
        })
        .collect())
}

/// Torrents started as audiobooks: info hash → book name. Only these may be streamed over
/// the LAN by `/audiobook/{id}/file/{idx}`; every other torrent stays behind the
/// loopback-only `/torrent/*` route. Kept by info hash and saved to `audiobooks.json` in
/// the state directory, because torrent ids are per session and the seeding rule must
/// still know a book after a restart.
static STARTED: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static STATE_FILE: Lazy<Mutex<Option<std::path::PathBuf>>> = Lazy::new(|| Mutex::new(None));

/// Read the saved list. Called once when the server starts.
pub fn load(state_dir: &std::path::Path) {
    let path = state_dir.join("audiobooks.json");
    let saved: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if let Ok(mut m) = STARTED.lock() {
        m.extend(saved);
    }
    if let Ok(mut f) = STATE_FILE.lock() {
        *f = Some(path);
    }
}

pub fn register(hash: &str, name: &str) {
    let Ok(mut m) = STARTED.lock() else { return };
    m.insert(hash.to_ascii_lowercase(), name.to_string());
    let Some(path) = STATE_FILE.lock().ok().and_then(|f| f.clone()) else {
        return;
    };
    if let Ok(t) = serde_json::to_string_pretty(&*m) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn name_for_hash(hash: &str) -> Option<String> {
    STARTED
        .lock()
        .ok()
        .and_then(|m| m.get(&hash.to_ascii_lowercase()).cloned())
}

/// The book name for a torrent id, if that torrent was started as an audiobook.
pub fn name_of(engine: &crate::torrent_engine::TorrentEngine, id: u32) -> Option<String> {
    name_for_hash(&engine.info_hash_hex(id)?)
}

const AUDIO_EXT: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "flac", "wav", "wma",
];

pub fn is_audio(path: &str) -> bool {
    path.rsplit_once('.')
        .map(|(_, e)| AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Sort key that compares runs of digits as numbers: "Part 2" < "Part 10".
pub fn natural_key(s: &str) -> Vec<(u8, u64, String)> {
    let mut out = Vec::new();
    let mut digits = String::new();
    let mut text = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_digit() {
            if !text.is_empty() {
                out.push((1, 0, std::mem::take(&mut text)));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                out.push((
                    0,
                    digits.parse().unwrap_or(u64::MAX),
                    std::mem::take(&mut digits),
                ));
            }
            text.push(c);
        }
    }
    if !digits.is_empty() {
        out.push((0, digits.parse().unwrap_or(u64::MAX), digits));
    }
    if !text.is_empty() {
        out.push((1, 0, text));
    }
    out
}

/// The audio files of a torrent's file list, in listening order.
pub fn listening_order(files: Vec<(usize, String, u64)>) -> Vec<(usize, String, u64)> {
    let mut v: Vec<_> = files.into_iter().filter(|(_, p, _)| is_audio(p)).collect();
    v.sort_by(|a, b| natural_key(&a.1).cmp(&natural_key(&b.1)));
    v
}

pub fn chapter_label(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.')
        .map(|(a, _)| a)
        .unwrap_or(file)
        .to_string()
}

/// A VLC playlist with every chapter in order, each streamed from spela.
pub fn vlc_playlist(
    base: &str,
    id: u32,
    book: &str,
    files: &[(usize, String, u64)],
) -> Result<String> {
    if files.is_empty() {
        return Err(anyhow!("This torrent holds no audio files"));
    }
    let mut m = String::from("#EXTM3U\n");
    for (idx, path, _) in files {
        m.push_str(&format!(
            "#EXTINF:-1,{} — {}\n#EXTVLCOPT:network-caching=3000\n{base}/audiobook/{id}/file/{idx}\n",
            crate::podcast::one_line(book),
            crate::podcast::one_line(&chapter_label(path))
        ));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apibay_rows_parse_and_the_empty_sentinel_is_dropped() {
        // Shape copied from a real answer (2026-11-04, q=Infinite Jest, cat=102).
        let v = serde_json::json!([
            {"id":"77638740","name":"Infinite Jest - David Foster Wallace - 2024 (miok) [Audiobook] (Humor)","info_hash":"A1EBFB303C4DEA16CB5A02644FBEC74B35DFD6DA","leechers":"1","seeders":"15","size":"1849545498","num_files":"7","username":"miok","added":"1735056384","status":"vip","category":"102","imdb":""},
            {"id":"0","name":"No results returned","info_hash":"0000000000000000000000000000000000000000","leechers":"0","seeders":"0","size":"0","num_files":"0"}
        ]);
        let b = parse_apibay(&v);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].hash, "a1ebfb303c4dea16cb5a02644fbec74b35dfd6da");
        assert_eq!(b[0].seeders, Some(15));
        assert_eq!(b[0].files, Some(7));
    }

    #[test]
    fn knaben_keeps_only_audiobooks() {
        // Shape copied from a real answer (2026-11-04, "Penguin and the Leviathan").
        let v = serde_json::json!({"hits":[
            {"bytes":198684180,"cachedOrigin":"The Pirate Bay (proxy)","category":"Audio / Audiobook","hash":"C25A4593DC2A9F0F6CBA05708825E4DD56724EDE","peers":1,"seeders":0,"title":"The Penguin and the Leviathan"},
            {"bytes":1000,"cachedOrigin":"The Pirate Bay (proxy)","category":"Books / Ebook","hash":"B25A4593DC2A9F0F6CBA05708825E4DD56724EDE","peers":3,"seeders":9,"title":"The Penguin and the Leviathan epub"}
        ]});
        let b = parse_knaben(&v);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].sources, vec!["The Pirate Bay".to_string()]);
    }

    #[test]
    fn abb_pages_give_title_hash_and_size() {
        let search = r#"<div class="postTitle"><h2><a href="/abss/the-end-of-eternity-isaac-asimov/" rel="bookmark">The End of Eternity - Isaac Asimov</a></h2></div>"#;
        assert_eq!(
            parse_abb_search(search),
            vec![(
                "/abss/the-end-of-eternity-isaac-asimov/".into(),
                "The End of Eternity - Isaac Asimov".into()
            )]
        );
        // Rows copied from the real book page (2026-11-04).
        let detail = "<td>Combined File Size:</td>\n<td>332.67 MBs</td>\n</tr>\n<td>Info Hash:</td>\n<td>1b3d4ff0c4992e37a1d84068ac5d8257004e1921</td>";
        let (h, s) = parse_abb_detail(detail).unwrap();
        assert_eq!(h, "1b3d4ff0c4992e37a1d84068ac5d8257004e1921");
        assert_eq!(s, Some(332_670_000));
    }

    #[test]
    fn merge_keeps_the_best_count_and_every_source() {
        let mk = |s: Option<u32>, src: &str| Book {
            hash: "a".repeat(40),
            name: "Sapiens A Brief History".into(),
            seeders: s,
            leechers: None,
            bytes: None,
            files: None,
            sources: vec![src.into()],
        };
        let v = merge_and_rank(
            vec![
                mk(Some(64), "The Pirate Bay"),
                mk(None, "AudioBook Bay"),
                mk(Some(75), "Knaben"),
            ],
            "sapiens",
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].seeders, Some(75));
        assert_eq!(v[0].sources.len(), 3);
    }

    #[test]
    fn ranking_puts_live_swarms_first_and_reported_zero_last() {
        let mk = |h: char, s: Option<u32>| Book {
            hash: h.to_string().repeat(40),
            name: "Debt The First 5000 Years".into(),
            seeders: s,
            leechers: None,
            bytes: None,
            files: None,
            sources: vec![],
        };
        let v = merge_and_rank(
            vec![
                mk('a', Some(0)),
                mk('b', None),
                mk('c', Some(3)),
                mk('d', Some(12)),
            ],
            "debt",
        );
        let order: Vec<_> = v.iter().map(|b| b.seeders).collect();
        assert_eq!(order, vec![Some(12), Some(3), None, Some(0)]);
    }

    #[test]
    fn the_query_must_be_in_the_name() {
        assert!(matches_query(
            "Can't Hurt Me by David Goggins (.M4B)",
            "Can't Hurt Me"
        ));
        assert!(matches_query(
            "Isaac Asimov's Guide to the Bible",
            "Asimov Guide to the Bible"
        ));
        assert!(matches_query(
            "Isaac Asimov's Guide to the Bible",
            "Guide to the Bible"
        ));
        assert!(!matches_query(
            "Resonance of Fate End of Eternity",
            "Infinite Jest"
        ));
    }

    #[test]
    fn hashes_are_validated() {
        assert!(valid_hash("A1EBFB303C4DEA16CB5A02644FBEC74B35DFD6DA").is_some());
        assert!(valid_hash("not-a-hash").is_none());
        assert!(valid_hash(&"g".repeat(40)).is_none());
    }

    #[test]
    fn chapters_come_in_listening_order_and_only_audio() {
        let files = vec![
            (0, "Book/Part 10.mp3".into(), 1),
            (1, "Book/cover.jpg".into(), 1),
            (2, "Book/Part 2.mp3".into(), 1),
            (3, "Book/Part 1.MP3".into(), 1),
            (4, "Book/notes.txt".into(), 1),
        ];
        let order: Vec<usize> = listening_order(files).into_iter().map(|f| f.0).collect();
        assert_eq!(order, vec![3, 2, 0]);
    }

    #[test]
    fn the_playlist_lists_every_chapter() {
        let files = vec![
            (3, "Book/Part 1.mp3".into(), 1),
            (2, "Book/Part 2.mp3".into(), 1),
        ];
        let m = vlc_playlist("http://host:7890", 9, "Book", &files).unwrap();
        assert!(m.contains("http://host:7890/audiobook/9/file/3\n"));
        assert!(m.find("/file/3").unwrap() < m.find("/file/2").unwrap());
        assert!(vlc_playlist("http://h", 1, "B", &[]).is_err());
    }

    #[test]
    fn magnets_carry_the_hash_name_and_trackers() {
        let m = magnet(&"a".repeat(40), "A Book & More");
        assert!(m.starts_with(&format!("magnet:?xt=urn:btih:{}", "a".repeat(40))));
        assert!(m.contains("dn=A%20Book%20%26%20More"));
        assert!(m.contains("&tr=udp%3A%2F%2F"));
        assert!(crate::torrent_engine::validate_magnet_uri(&m).is_ok());
    }

    /// The three real sources with real titles. Network, so ignored by default:
    /// `cargo test audiobook -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_sources_find_known_books() {
        for q in ["Sapiens", "The Body Keeps the Score", "The End of Eternity"] {
            let r = search(q).await;
            assert!(
                !r.books.is_empty(),
                "nothing for {q}; failed sources: {:?}",
                r.failed
            );
        }
    }
}
