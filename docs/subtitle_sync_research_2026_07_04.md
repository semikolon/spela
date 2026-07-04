# Subtitle-Audio Sync — the ingenious guarantee (research 2026-07-04)

Research deliverable answering "is there an ingenious way to GUARANTEE subtitles
are always in sync with the audio?" Sources cited inline. NOT YET IMPLEMENTED —
this is the design for the follow-up. Verbatim subagent synthesis condensed to
the actionable core.

## Executive answer
You cannot "guarantee" sync from an arbitrary external SRT — the guarantee comes
only from subtitles that share the source file's timeline. Correct architecture
is a **preference cascade**:

1. **Embedded TEXT subtitle track from the exact release MKV → inherently
   perfectly synced. Extract via `ffmpeg -c:s copy`, burn AS-IS (no alignment).**
   Cue times and audio PTS share one muxed clock. (spela already does this when
   embedded subs are present — the reason the Danish source played synced.)
2. **External SRT (OpenSubtitles) → NEVER burn as-fetched. Align first** with
   `alass` against the CHOSEN AUDIO TRACK (or, faster + more accurate, an
   embedded subtitle reference in any language).
3. **Nothing usable → burn nothing / (future) ASR-generate.**

## Tool choice: `alass` (Rust), shell out
`alass` (kaegi/alass, "Automatic Language-Agnostic Subtitle Synchronization") —
VAD speech-activity + dynamic-programming search over **offset + framerate +
split points**. Single portable Rust binary (matches spela's stack), no Python.
Handles: (a) constant offset ✅, (b) framerate 23.976↔25 drift ✅, (c) mid-file
splits (ad breaks/cuts) ✅ (its defining feature; `--split-penalty` default 7,
`--no-splits` for fast offset+framerate-only mode). Accuracy (author benchmark
N=118): 80% of lines within 100 ms, 95% within 800 ms. Install
`cargo install alass-cli`; point `ALASS_FFMPEG_PATH`/`ALASS_FFPROBE_PATH` at the
binaries; use the plain (non-`ffmpeg-library`) build.

`ffsubsync` (Python, VAD+FFT cross-correlation, `--vad=silero`, `--gss` for
arbitrary framerate) — handles offset+framerate but NOT mid-file splits. Keep
documented as a manual fallback; rejected as default (Python dep, no splits).

ASR forced-alignment (WhisperX, stable-ts DTW, NeMo NFA, Qwen3-ForcedAligner-0.6B
Jan 2026) is the accuracy ceiling and the genuinely-new 2026 direction, but
GPU-heavy + per-language models → wrong tool for RE-TIMING an existing SRT before
a near-live NVENC transcode. Reserve for a future "generate subs when none
exist" feature.

## spela architecture (drop-in for subtitles.rs/transcode.rs)
```
resolve_subtitle(video, chosen_audio_idx, lang):
  if embedded_text_track(lang) exists:
      extract -c:s copy -> return (PERFECT, no align)              # rung 1
  external = fetch_opensubtitles(lang)
  if any embedded_text_track exists (any lang):
      ref = extract that track (-c:s copy)
      aligned = alass(ref, external)         # sub-second sub-to-sub  # rung 2
  else:
      ref_wav = ffmpeg -map 0:a:{chosen_audio_idx} -ac 1 -ar 16000 -vn -f wav
      aligned = alass(ref_wav, external, --no-splits first)          # rung 3
  validate offset<max & framerate plausible, else -> next result
  cache(aligned, key=<imdb>_s..e.._<lang>); return aligned
  # caller then applies shift_srt(ss_offset) THEN -vf subtitles=
```
- **Align against the chosen audio track**, not stream 0 — pre-extract a mono
  16 kHz wav of `0:a:{audio_index}` so English subs never align to a Russian dub.
- Rung 2 (embedded reference) is **< 1 s** (no audio decode). Rung 3 (audio) is
  ~5-10 s CPU-side (no GPU contention), fits inside the existing 20 s pre-buffer.
- **Cache** the aligned SRT (deterministic per video+srt) → re-plays skip it.
- **Bound the correction**: offset > 60 s or implausible framerate → treat as a
  failed match, prefer the next search result's SRT (mirrors the ranker's
  auto-retry philosophy).

## CRITICAL spela-specific diagnostic (check BEFORE blaming the SRT)
A constant lag that is the **same across every title** points at spela's OWN
handling, not OpenSubtitles:
- **`-ss` input-seek resets PTS.** The `subtitles` filter reads cue times
  literally against the re-based input → any burned sub (embedded OR external)
  must be shifted by the same `ss_offset`. Order matters: **align → THEN
  shift_srt(ss_offset) → THEN `-vf subtitles=`.**
- **Container audio `start_time` offset** (Amazon WEB-DL notably) — check
  `ffprobe` audio stream `start_time` vs subtitle stream. Subgen shipped an
  explicit fix for this (2026-04-11).
- **Image-based tracks (PGS/VobSub)** have `width`/`height` in ffprobe, are NOT
  text, can't be `-c:s copy`'d to SRT nor fed to `-vf subtitles=` → need OCR or
  skip to external. Detect: text tracks lack `width`/`height`.
- `-map 0:m:language:eng` errors if >1 eng track → always have a by-index
  fallback from an `ffprobe -select_streams s` scan.

The observed ~10 s lag was on the MULTI English-dub play; the Danish source
(embedded subs, same-file timeline) played synced — so for spela the embedded-
first rung already solves the common case. The alass rung fixes external-SRT
fallbacks. Investigate the ss_offset/start_time path if any constant lag recurs
on embedded subs.

## Sources
- alass: github.com/kaegi/alass/blob/master/README.md · lib.rs/crates/alass-cli
- ffsubsync: github.com/smacke/ffsubsync · ffsubsync.readthedocs.io
- alass vs ffsubsync splits: github.com/SubtitleEdit/subtitleedit/discussions/8222
- embedded-first + audio-extract cost: wiki.bazarr.media/Additional-Configuration/Performance-Tuning
- ffmpeg extract (`-c:s copy`, `0:s:N`, image-vs-text): nicolasbouliane.com/blog/ffmpeg-extract-subtitles · mux.com/articles/extracting-subtitles-and-captions-from-video-files-with-ffmpeg
- 2026 ASR/forced-alignment: github.com/jianfch/stable-ts · Qwen3-ASR report arxiv.org/html/2601.21337v2 · github.com/McCloudS/subgen (2026-04 audio-offset fix, /subsync)
