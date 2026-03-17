use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

const OPENSUBTITLES_V3: &str = "https://opensubtitles-v3.strem.io/subtitles";

/// Fetch subtitles from Stremio OpenSubtitles v3 addon (zero auth).
/// Downloads SRT, converts to VTT, saves to media dir.
pub async fn fetch_subtitles(
    client: &reqwest::Client,
    imdb_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
    lang: &str,
    media_dir: &Path,
) -> Result<Option<PathBuf>> {
    let (id, media_type) = match (season, episode) {
        (Some(s), Some(e)) => (format!("{}:{}:{}", imdb_id, s, e), "series"),
        _ => (imdb_id.to_string(), "movie"),
    };

    let url = format!("{}/{}/{}.json", OPENSUBTITLES_V3, media_type, id);
    let resp: Value = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.json().await?,
        _ => return Ok(None),
    };

    let subs = resp["subtitles"].as_array()
        .map(|arr| arr.iter().filter(|s| s["lang"].as_str() == Some(lang)).collect::<Vec<_>>())
        .unwrap_or_default();

    if subs.is_empty() { return Ok(None); }

    let sub_url = match subs[0]["url"].as_str() {
        Some(u) => u,
        None => return Ok(None),
    };

    let srt_text = match client.get(sub_url).send().await {
        Ok(r) if r.status().is_success() => r.text().await?,
        _ => return Ok(None),
    };

    // Save SRT
    std::fs::create_dir_all(media_dir)?;
    let srt_path = media_dir.join(format!("subtitle_{}.srt", lang));
    std::fs::write(&srt_path, &srt_text)?;

    // Convert to VTT for Chromecast
    let vtt_text = srt_to_vtt(&srt_text);
    let vtt_path = media_dir.join(format!("subtitle_{}.vtt", lang));
    std::fs::write(&vtt_path, &vtt_text)?;

    Ok(Some(vtt_path))
}

/// Convert SRT subtitle format to WebVTT.
fn srt_to_vtt(srt: &str) -> String {
    let mut vtt = String::from("WEBVTT\n\n");
    for line in srt.lines() {
        // SRT uses comma for milliseconds, VTT uses dot
        if line.contains(" --> ") {
            vtt.push_str(&line.replace(',', "."));
        } else {
            vtt.push_str(line);
        }
        vtt.push('\n');
    }
    vtt
}
