//! Save an audiobook to the Books app on macOS (2026-11-04).
//!
//! Runs on the listener's Mac, as a client of the spela server: it waits for the book's
//! torrent to finish, fetches every chapter over the LAN, joins them into ONE `.m4b` with a
//! chapter mark per file, and opens it with Books. One file with chapters is what Books
//! handles best; separate MP3s are reported to import as many titles, or merge into one
//! long title without chapter marks.
//!
//! The encoder is ffmpeg's `aac_at` (Apple's own AAC, present on macOS builds), falling
//! back to ffmpeg's `aac`. A book that already is one `.m4b`/`.m4a` file is passed through
//! without re-encoding.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A chapter as the joined file will name it.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub millis: u64,
}

/// ffmpeg's metadata file: the book's title and one `[CHAPTER]` per input, laid end to end.
pub fn ffmetadata(book: &str, chapters: &[Chapter]) -> String {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('=', "\\=")
            .replace(';', "\\;")
            .replace('#', "\\#")
            .replace('\n', " ")
    };
    let mut m = format!(
        ";FFMETADATA1\ntitle={}\nalbum={}\ngenre=Audiobook\n",
        esc(book),
        esc(book)
    );
    let mut start = 0u64;
    for c in chapters {
        let end = start + c.millis;
        m.push_str(&format!(
            "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={}\n",
            esc(&c.title)
        ));
        start = end;
    }
    m
}

/// Chapter names from file names: what every file shares at the start and end is the
/// book's name or a bitrate tag, so it is dropped ("heart_of_darkness_1a_conrad_64kb" →
/// "1a"), and underscores become spaces. A name left empty becomes "Chapter N".
pub fn chapter_titles(labels: &[String]) -> Vec<String> {
    let split = |s: &str| -> Vec<String> {
        s.split(['_', ' ', '-', '.'])
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect()
    };
    let toks: Vec<Vec<String>> = labels.iter().map(|l| split(l)).collect();
    if toks.len() < 2 {
        return labels.iter().map(|l| l.replace('_', " ")).collect();
    }
    let min = toks.iter().map(|t| t.len()).min().unwrap_or(0);
    let pre = (0..min)
        .take_while(|&i| toks.iter().all(|t| t[i] == toks[0][i]))
        .count();
    let suf = (0..min - pre)
        .take_while(|&i| {
            toks.iter()
                .all(|t| t[t.len() - 1 - i] == toks[0][toks[0].len() - 1 - i])
        })
        .count();
    toks.iter()
        .enumerate()
        .map(|(n, t)| {
            let mid = t[pre..t.len() - suf].join(" ");
            if mid.is_empty() {
                format!("Chapter {}", n + 1)
            } else if mid
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
                && mid.len() <= 4
            {
                format!("Part {mid}")
            } else {
                mid
            }
        })
        .collect()
}

/// A file name Books and Finder accept, from a release name.
pub fn file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.,()'&".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = joined.chars().take(120).collect();
    if t.is_empty() {
        "Audiobook".into()
    } else {
        t
    }
}

fn ffprobe_millis(path: &Path) -> Result<u64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .context("ffprobe not found")?;
    let s = String::from_utf8_lossy(&out.stdout);
    let secs: f64 = s
        .trim()
        .parse()
        .map_err(|_| anyhow!("no duration for {}", path.display()))?;
    Ok((secs * 1000.0).round() as u64)
}

fn ffprobe_channels(path: &Path) -> u32 {
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=channels",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(2)
}

fn has_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        })
        .unwrap_or(false)
}

/// Join chapter files into one `.m4b` at `out`.
pub fn join_to_m4b(book: &str, files: &[(PathBuf, String)], out: &Path, work: &Path) -> Result<()> {
    let mut chapters = Vec::with_capacity(files.len());
    let mut list = String::new();
    let mut channels = 1;
    for (path, title) in files {
        chapters.push(Chapter {
            title: title.clone(),
            millis: ffprobe_millis(path)?,
        });
        channels = channels.max(ffprobe_channels(path));
        list.push_str(&format!(
            "file '{}'\n",
            path.display().to_string().replace('\'', "'\\''")
        ));
    }
    let list_path = work.join("chapters.txt");
    let meta_path = work.join("metadata.txt");
    std::fs::write(&list_path, list)?;
    std::fs::write(&meta_path, ffmetadata(book, &chapters))?;
    let encoder = if has_encoder("aac_at") {
        "aac_at"
    } else {
        "aac"
    };
    let bitrate = if channels <= 1 { "64k" } else { "96k" };
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_path)
        .arg("-i")
        .arg(&meta_path)
        .args([
            "-map",
            "0:a",
            "-map_metadata",
            "1",
            "-map_chapters",
            "1",
            "-vn",
        ])
        .args([
            "-c:a",
            encoder,
            "-b:a",
            bitrate,
            "-ar",
            "44100",
            "-ac",
            &channels.min(2).to_string(),
        ])
        .args([
            "-metadata",
            "media_type=2",
            "-movflags",
            "+faststart",
            "-f",
            "ipod",
        ])
        .arg(out)
        .status()
        .context("ffmpeg not found")?;
    if !status.success() {
        bail!("ffmpeg could not join the chapters");
    }
    Ok(())
}

/// Books saved by `save`, so `finished` knows which Books entries came from spela:
/// `~/.config/spela/books_saved.json`, info hash → book name.
fn saved_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/spela/books_saved.json")
}

fn load_saved() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(saved_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store_saved(m: &std::collections::BTreeMap<String, String>) {
    let p = saved_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(t) = serde_json::to_string_pretty(m) {
        let _ = std::fs::write(p, t);
    }
}

/// The Books app's library database. The newest `BKLibrary-*.sqlite` is the live one.
fn books_db() -> Option<PathBuf> {
    let dir =
        dirs::home_dir()?.join("Library/Containers/com.apple.iBooksX/Data/Documents/BKLibrary");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("BKLibrary-") && n.ends_with(".sqlite"))
        })
        .collect();
    v.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    v.pop()
}

/// How far Books says the listener is through an audiobook of this title:
/// `(finished, progress 0..1)`, or `None` if Books has no audiobook by that title.
/// `ZCONTENTTYPE = 6` is Books' audiobook kind (seen on an import, 2026-11-04).
pub fn books_progress(title: &str) -> Option<(bool, f64)> {
    let db = books_db()?;
    let q = format!(
        "select coalesce(ZISFINISHED,0), coalesce(ZREADINGPROGRESS,0) from ZBKLIBRARYASSET \
         where ZCONTENTTYPE = 6 and ZTITLE = '{}' limit 1;",
        title.replace('\'', "''")
    );
    let out = Command::new("sqlite3")
        .args(["-readonly", "-separator", "|"])
        .arg(&db)
        .arg(q)
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let (f, p) = line.trim().split_once('|')?;
    Some((f.trim() == "1", p.trim().parse().unwrap_or(0.0)))
}

/// Finished by Books' own mark, or listened to the end without the mark.
pub fn is_done(finished: bool, progress: f64) -> bool {
    finished || progress >= 0.99
}

/// List the books spela saved to Books with how far through each one is; with `remove`,
/// delete the torrent and files of every finished one on the server.
pub async fn finished(base: &str, remove: bool) -> Result<Value> {
    let client = reqwest::Client::new();
    let mut saved = load_saved();
    let mut rows = Vec::new();
    let mut removed = Vec::new();
    for (hash, name) in saved.clone() {
        let state = books_progress(&name);
        let done = state.map(|(f, p)| is_done(f, p)).unwrap_or(false);
        rows.push(serde_json::json!({
            "book": name, "hash": hash, "in_books": state.is_some(),
            "progress": state.map(|s| (s.1 * 100.0).round() / 100.0), "finished": done,
        }));
        if remove && done {
            let r: Value = client
                .post(format!("{base}/audiobook/remove"))
                .json(&serde_json::json!({ "hash": hash }))
                .send()
                .await?
                .json()
                .await?;
            if r.get("error").is_none() {
                saved.remove(&hash);
                removed.push(name.clone());
            } else {
                eprintln!("could not remove {name}: {}", r["error"]);
            }
        }
    }
    if remove {
        store_saved(&saved);
    }
    Ok(serde_json::json!({ "books": rows, "removed": removed,
        "note": "Books keeps its own copy; remove a finished book there by hand to free that space too." }))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    Ok(client.get(url).send().await?.json().await?)
}

/// Fetch, join and (unless `import` is false) open the book with Books. Answers the path
/// of the `.m4b` that was written.
pub async fn save(
    base: &str,
    hash: &str,
    name: &str,
    import: bool,
    out_dir: &Path,
) -> Result<Value> {
    let client = reqwest::Client::new();
    let started: Value = client
        .post(format!("{base}/audiobook/start"))
        .json(&serde_json::json!({"hash": hash, "name": name}))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = started.get("error") {
        bail!("{}", e);
    }
    let id = started["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("no torrent id"))?;

    // Wait for the whole book. Give up only when nothing has arrived for five minutes.
    let mut last = (Instant::now(), 0u64);
    let files = loop {
        let f = get_json(&client, &format!("{base}/audiobook/{id}/files")).await?;
        let done = f["progress"]["downloaded"].as_u64().unwrap_or(0);
        let total = f["progress"]["total"].as_u64().unwrap_or(0);
        if f["ready"].as_bool() == Some(true) && total > 0 && done >= total {
            break f["files"].as_array().cloned().unwrap_or_default();
        }
        if done > last.1 {
            last = (Instant::now(), done);
        } else if last.0.elapsed() > Duration::from_secs(300) {
            bail!("no download progress for five minutes ({done} of {total} bytes)");
        }
        eprintln!(
            "downloading: {} of {} MB",
            done / 1_000_000,
            total / 1_000_000
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    if files.is_empty() {
        bail!("the torrent holds no audio files");
    }

    let work = std::env::temp_dir().join(format!("spela-book-{hash}"));
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(out_dir)?;
    let mut local = Vec::new();
    for (n, f) in files.iter().enumerate() {
        let idx = f["idx"]
            .as_u64()
            .ok_or_else(|| anyhow!("chapter without index"))?;
        let label = f["label"].as_str().unwrap_or("Chapter").to_string();
        let ext = f["ext"]
            .as_str()
            .filter(|e| !e.is_empty())
            .unwrap_or("mp3")
            .to_string();
        let bytes = client
            .get(format!("{base}/audiobook/{id}/file/{idx}"))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let p = work.join(format!("{n:04}.{ext}"));
        std::fs::write(&p, &bytes)?;
        local.push((p, label, ext));
    }

    let out = out_dir.join(format!("{}.m4b", file_stem(name)));
    // One MP4 audio file already is what Books wants: copy it, no re-encode.
    if local.len() == 1 && matches!(local[0].2.as_str(), "m4b" | "m4a") {
        std::fs::copy(&local[0].0, &out)?;
    } else {
        let titles = chapter_titles(&local.iter().map(|(_, l, _)| l.clone()).collect::<Vec<_>>());
        let parts: Vec<(PathBuf, String)> = local
            .iter()
            .zip(titles)
            .map(|((p, _, _), t)| (p.clone(), t))
            .collect();
        join_to_m4b(name, &parts, &out, &work)?;
    }
    let _ = std::fs::remove_dir_all(&work);

    let mut in_books = false;
    if import {
        let st = Command::new("open")
            .args(["-a", "Books"])
            .arg(&out)
            .status()?;
        if !st.success() {
            bail!("Books did not open {}", out.display());
        }
        // Books copies an imported audiobook into its own storage. Once its library lists
        // the title, this copy is spare and the Mac's disk gets it back.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if books_progress(name).is_some() {
                in_books = true;
                let _ = std::fs::remove_file(&out);
                break;
            }
        }
    }
    let mut saved = load_saved();
    saved.insert(hash.to_ascii_lowercase(), name.to_string());
    store_saved(&saved);
    Ok(
        serde_json::json!({"book": name, "file": if in_books { Value::Null } else { serde_json::json!(out) },
        "chapters": local.len(), "opened_in_books": import, "in_books": in_books}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapters_are_laid_end_to_end() {
        let m = ffmetadata(
            "A Book",
            &[
                Chapter {
                    title: "One".into(),
                    millis: 1000,
                },
                Chapter {
                    title: "Two = 2".into(),
                    millis: 500,
                },
            ],
        );
        assert!(m.starts_with(";FFMETADATA1\ntitle=A Book\n"));
        assert!(m.contains("START=0\nEND=1000\ntitle=One"));
        assert!(m.contains("START=1000\nEND=1500\ntitle=Two \\= 2"));
        assert!(m.contains("genre=Audiobook"));
    }

    #[test]
    fn chapter_titles_drop_what_every_file_shares() {
        let l: Vec<String> = [
            "heart_of_darkness_1a_conrad_64kb",
            "heart_of_darkness_1b_conrad_64kb",
            "heart_of_darkness_2a_conrad_64kb",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(chapter_titles(&l), vec!["Part 1a", "Part 1b", "Part 2a"]);
        let l: Vec<String> = [
            "01 The Magic of Everyday Communication",
            "02 The Complex Layers of Talk",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            chapter_titles(&l),
            vec![
                "01 The Magic of Everyday Communication",
                "02 The Complex Layers of Talk"
            ]
        );
        assert_eq!(
            chapter_titles(&["Same".to_string(), "Same".to_string()]),
            vec!["Chapter 1", "Chapter 2"]
        );
    }

    #[test]
    fn a_book_is_done_when_marked_or_heard_to_the_end() {
        assert!(is_done(true, 0.2));
        assert!(is_done(false, 0.995));
        assert!(!is_done(false, 0.5));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(
            file_stem("Audiobook - The Heart of Darkness - Joseph Conrad [Librivox]"),
            "Audiobook - The Heart of Darkness - Joseph Conrad Librivox"
        );
        assert_eq!(file_stem("a/b:c"), "a b c");
        assert_eq!(file_stem("///"), "Audiobook");
    }
}
