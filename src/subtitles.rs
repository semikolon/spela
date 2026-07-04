use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

const OPENSUBTITLES_V3: &str = "https://opensubtitles-v3.strem.io/subtitles";

/// Apr 28, 2026: ISO 639-1 → ISO 639-2 mapping for embedded-track matching.
/// MKV/Matroska conventionally tags subtitle streams with 3-letter codes
/// (eng/ger/spa/fre/...) while spela's CLI/config uses 2-letter codes
/// (en/de/es/fr/...). Without this translation, we never match an embedded
/// track and fall straight through to OpenSubtitles.
fn iso639_1_to_2(lang: &str) -> &str {
    match lang {
        "en" => "eng",
        "de" => "ger",
        "es" => "spa",
        "fr" => "fre",
        "it" => "ita",
        "pt" => "por",
        "ru" => "rus",
        "ja" => "jpn",
        "ko" => "kor",
        "zh" => "chi",
        "ar" => "ara",
        "nl" => "dut",
        "sv" => "swe",
        "no" => "nor",
        "da" => "dan",
        "fi" => "fin",
        "pl" => "pol",
        "tr" => "tur",
        "cs" => "cze",
        "el" => "gre",
        "he" => "heb",
        "hi" => "hin",
        "hu" => "hun",
        "id" => "ind",
        "th" => "tha",
        "vi" => "vie",
        "uk" => "ukr",
        // Pass through anything else verbatim — the user might already be
        // using a 3-letter code, or the source might use an exotic tag.
        other => other,
    }
}

/// Apr 28, 2026: Try to extract an English-translation subtitle track from
/// the source MKV. Returns `Ok(true)` if we successfully wrote a non-trivial
/// SRT file to `dest`. Caller should then fall back to OpenSubtitles on
/// `Ok(false)` or `Err(_)`.
///
/// Preference order is shaped by what Apple TV+ / streaming WEB-DLs ship:
///
///  1. **Forced track** (`disposition.forced=1`) — only translates
///     non-primary-language dialogue (e.g., German speech inside an
///     English-language show like Hijack). This is what the user actually
///     wants for Hijack S2E1: subtitles appear ONLY when characters speak
///     German, no captions for the English speech they can already hear.
///     Apple TV+ marks this track `default=1` for shows with foreign-
///     language passages — that's how it knows to auto-display.
///  2. **Full non-SDH track** — translates everything (English + foreign).
///     Used when the show has no foreign passages (forced track absent or
///     empty), so we still get something useful to burn in.
///  3. **SDH track** — fallback if neither of the above. SDH includes
///     ambient sound notations like "[airplane passing overhead]" plus
///     speaker IDs, which some users dislike for general viewing — but
///     it's better than no subtitles, and SOME SDH tracks DO include
///     foreign-language translations alongside the captions.
///
/// Bug this fixes (Apr 28, 2026): OpenSubtitles' English file for Hijack
/// S2E1 was an SDH variant that NOTATES "[in German]" / "[officers speak
/// German]" without translating, because that's what SDH is FOR (deaf
/// viewers can't hear the language to know what's said). Apple TV+'s
/// own forced track DOES translate the German. Extracting the embedded
/// forced track gets the user the translation Apple shipped originally.
async fn extract_embedded_subtitle(source: &Path, lang: &str, dest_srt: &Path) -> Result<bool> {
    use tokio::process::Command;

    // List subtitle streams in the source.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "s",
        ])
        .arg(source)
        .output()
        .await?;

    if !probe.status.success() {
        anyhow::bail!("ffprobe failed: {}", String::from_utf8_lossy(&probe.stderr));
    }

    let probe_json: Value = serde_json::from_slice(&probe.stdout)?;
    let streams = match probe_json["streams"].as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Ok(false),
    };

    let lang_iso2 = iso639_1_to_2(lang);

    // Helper: does this stream's language tag match the requested lang?
    let stream_lang_matches = |s: &&Value| -> bool {
        s["tags"]["language"]
            .as_str()
            .map(|l| l.eq_ignore_ascii_case(lang_iso2))
            .unwrap_or(false)
    };

    let title_contains_sdh = |s: &&Value| -> bool {
        s["tags"]["title"]
            .as_str()
            .map(|t| t.to_uppercase().contains("SDH") || t.to_uppercase().contains("HEARING"))
            .unwrap_or(false)
    };

    // Tier 1: forced + matching language
    let forced = streams
        .iter()
        .find(|s| stream_lang_matches(s) && s["disposition"]["forced"].as_i64() == Some(1));

    // Tier 2: matching language, NOT forced, NOT SDH (full clean translation)
    let full = streams.iter().find(|s| {
        stream_lang_matches(s)
            && s["disposition"]["forced"].as_i64() != Some(1)
            && !title_contains_sdh(s)
    });

    // Tier 3: SDH fallback (still better than nothing)
    let sdh = streams
        .iter()
        .find(|s| stream_lang_matches(s) && title_contains_sdh(s));

    for (label, candidate) in [("forced", forced), ("full", full), ("sdh", sdh)] {
        let Some(stream) = candidate else { continue };
        let Some(abs_idx) = stream["index"].as_i64() else {
            continue;
        };

        // Extract this track via ffmpeg.
        let map_arg = format!("0:{abs_idx}");
        let out = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(source)
            .args(["-map", &map_arg, "-c:s", "srt"])
            .arg(dest_srt)
            .output()
            .await?;

        if !out.status.success() {
            tracing::warn!(
                "embedded subtitle extract via {label} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            continue;
        }

        // Verify the file has meaningful content. A forced track with no
        // foreign passages in this episode could legitimately be near-empty,
        // in which case fall through to the next tier.
        let size = std::fs::metadata(dest_srt).map(|m| m.len()).unwrap_or(0);
        if size > 200 {
            tracing::info!(
                "Subtitles extracted from source MKV (track {abs_idx}, {label}, {size} bytes)"
            );
            return Ok(true);
        } else {
            tracing::info!(
                "Subtitle track {abs_idx} ({label}) too small ({size} bytes) — trying next tier"
            );
        }
    }

    Ok(false)
}

/// Fetch subtitles. Apr 28, 2026: now prefers the source MKV's embedded
/// English-translation track over OpenSubtitles when a local source file
/// is provided and contains a usable track. Falls back to the original
/// OpenSubtitles fetch if extraction yields no usable file.
///
/// `source_path` — path to the source media file (typically the MKV from
/// webtorrent download). Pass `None` to skip embedded extraction.
pub async fn fetch_subtitles(
    client: &reqwest::Client,
    imdb_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
    lang: &str,
    media_dir: &Path,
    source_path: Option<&Path>,
) -> Result<Option<PathBuf>> {
    // Apr 28, 2026: try the source MKV's embedded forced/non-SDH track first.
    // Apple TV+ / streaming WEB-DL releases ship purpose-built tracks that
    // translate foreign-language passages (e.g., German speech in Hijack
    // S2E1) — OpenSubtitles' English file is frequently an SDH variant
    // that only NOTATES "[in German]" without translating. Embedded > OS
    // for any English-translation use case where the source has tracks.
    if let Some(src) = source_path {
        if src.exists() {
            std::fs::create_dir_all(media_dir)?;
            let srt_path = media_dir.join(format!("subtitle_{}.srt", lang));
            match extract_embedded_subtitle(src, lang, &srt_path).await {
                Ok(true) => {
                    let srt_text = std::fs::read_to_string(&srt_path).unwrap_or_default();
                    let vtt_text = srt_to_vtt(&srt_text);
                    let vtt_path = media_dir.join(format!("subtitle_{}.vtt", lang));
                    std::fs::write(&vtt_path, &vtt_text)?;
                    return Ok(Some(vtt_path));
                }
                Ok(false) => {
                    tracing::info!(
                        "No usable embedded {lang} subtitle track in source MKV — falling back to OpenSubtitles"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Embedded subtitle extraction errored: {e} — falling back to OpenSubtitles"
                    );
                }
            }
        } else {
            tracing::debug!("Source path {src:?} does not exist — skipping embedded extraction");
        }
    }

    let (id, media_type) = match (season, episode) {
        (Some(s), Some(e)) => (format!("{}:{}:{}", imdb_id, s, e), "series"),
        _ => (imdb_id.to_string(), "movie"),
    };

    let url = format!("{}/{}/{}.json", OPENSUBTITLES_V3, media_type, id);
    let resp: Value = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.json().await?,
        _ => return Ok(None),
    };

    let subs = resp["subtitles"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|s| s["lang"].as_str() == Some(lang))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if subs.is_empty() {
        return Ok(None);
    }

    let sub_url = match subs[0]["url"].as_str() {
        Some(u) => u,
        None => return Ok(None),
    };

    let srt_text = match client.get(sub_url).send().await {
        Ok(r) if r.status().is_success() => r.text().await?,
        _ => return Ok(None),
    };

    // Save the raw external SRT.
    std::fs::create_dir_all(media_dir)?;
    let srt_path = media_dir.join(format!("subtitle_{}.srt", lang));
    std::fs::write(&srt_path, &srt_text)?;

    // 2026-07-04: an external OpenSubtitles SRT is NOT guaranteed to share this
    // release's timeline — it can be off by a constant offset, a framerate
    // ratio (23.976↔25), or cut differently. When we have the local source
    // file, ALIGN it against the media (via `alass`) before burn-in so the
    // subtitles stay in sync with the audio. No local source (partial torrent
    // HTTP stream) → no alignment reference → burn as-fetched. Full rationale:
    // docs/subtitle_sync_research_2026_07_04.md.
    if let Some(src) = source_path {
        if src.exists() {
            let aligned = media_dir.join(format!("subtitle_{}.aligned.srt", lang));
            if align_srt_with_alass(src, &srt_path, &aligned, media_dir).await {
                // do_play burns `subtitle_{lang}.srt`, so replace it in place.
                let _ = std::fs::rename(&aligned, &srt_path);
            }
        }
    }

    // Convert the (now possibly-aligned) SRT to VTT for the Cast Receiver path.
    let final_srt = std::fs::read_to_string(&srt_path).unwrap_or(srt_text);
    let vtt_text = srt_to_vtt(&final_srt);
    let vtt_path = media_dir.join(format!("subtitle_{}.vtt", lang));
    std::fs::write(&vtt_path, &vtt_text)?;

    Ok(Some(vtt_path))
}

/// 2026-07-04: locate the alass binary. `cargo install alass-cli` installs it
/// as `alass-cli` (some distro packages call it `alass`). Prefers
/// `~/.cargo/bin/alass-cli`, then those names on PATH. If none runs, the caller
/// degrades gracefully to the unaligned SRT.
fn alass_binary() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        for name in ["alass-cli", "alass"] {
            let p = PathBuf::from(&home).join(".cargo/bin").join(name);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("alass-cli")
}

/// Align an external SRT against the media timeline with `alass`
/// (kaegi/alass — VAD speech-activity + dynamic-programming search over
/// offset + framerate). Returns true iff a non-trivial aligned file was
/// written to `out_srt`.
///
/// Reference preference (docs/subtitle_sync_research_2026_07_04.md):
///   1. An embedded TEXT subtitle track (any language) → sub-to-sub align,
///      <1s, most accurate (no audio decode).
///   2. Else the source media file → alass extracts audio itself (~5-10s CPU).
///
/// `--no-splits`: release files have no ad-breaks, so offset+framerate-only is
/// the correct model and avoids over-segmentation. Any failure (alass missing,
/// non-zero exit, trivial output) returns false → caller keeps the unaligned
/// SRT (never worse than today's behaviour).
async fn align_srt_with_alass(
    source: &Path,
    external_srt: &Path,
    out_srt: &Path,
    work_dir: &Path,
) -> bool {
    use tokio::process::Command;

    let alass = alass_binary();

    // Prefer an embedded text-subtitle track as the reference (fast + accurate,
    // no audio decode). Fall back to the media file (alass extracts audio).
    let reference = match extract_any_text_subtitle_ref(source, work_dir).await {
        Some(p) => p,
        None => source.to_path_buf(),
    };

    let out = Command::new(&alass)
        .arg("--no-splits")
        .arg(&reference)
        .arg(external_srt)
        .arg(out_srt)
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            let size = std::fs::metadata(out_srt).map(|m| m.len()).unwrap_or(0);
            if size > 50 {
                tracing::info!("alass aligned external subtitle ({size} bytes)");
                true
            } else {
                tracing::warn!("alass produced trivial output ({size} bytes) — keeping unaligned SRT");
                false
            }
        }
        Ok(o) => {
            tracing::warn!(
                "alass alignment failed (keeping unaligned SRT): {}",
                String::from_utf8_lossy(&o.stderr)
            );
            false
        }
        Err(e) => {
            tracing::debug!("alass not runnable ({e}) — keeping unaligned SRT");
            false
        }
    }
}

/// Extract ANY embedded TEXT subtitle track (first non-image track) to an SRT
/// for use as a fast, audio-free alignment reference. Image tracks (PGS/VobSub,
/// which carry `width`/`height`) are skipped — they can't be `-c:s srt`'d.
/// Writes into `work_dir` (spela-owned, writable) — the source may live on a
/// read-only library drive. Returns the ref path on non-trivial output.
async fn extract_any_text_subtitle_ref(source: &Path, work_dir: &Path) -> Option<PathBuf> {
    use tokio::process::Command;

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "s",
        ])
        .arg(source)
        .output()
        .await
        .ok()?;
    if !probe.status.success() {
        return None;
    }
    let j: Value = serde_json::from_slice(&probe.stdout).ok()?;
    let streams = j["streams"].as_array()?;
    // First TEXT track (no width/height → not image-based).
    let idx = streams.iter().find_map(|s| {
        let is_text = s["width"].as_i64().is_none() && s["height"].as_i64().is_none();
        if is_text {
            s["index"].as_i64()
        } else {
            None
        }
    })?;

    let ref_srt = work_dir.join(".alass_ref.srt");
    let out = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(source)
        .args(["-map", &format!("0:{idx}"), "-c:s", "srt"])
        .arg(&ref_srt)
        .output()
        .await
        .ok()?;
    if out.status.success() && std::fs::metadata(&ref_srt).map(|m| m.len()).unwrap_or(0) > 100 {
        Some(ref_srt)
    } else {
        None
    }
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
