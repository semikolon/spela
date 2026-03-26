# Tasks: Custom Cast Receiver for spela

## Overview
- **Estimated scope**: M (2-4 hours implementation, ~1 hour testing)
- **External dependency**: Google Cast SDK registration ($5, instant, 5-15 min activation)
- **Cut first if needed**: Resume memory (US-4), rich metadata overlay (US-5 poster images)

---

## Implementation Tasks

### Phase 1: Receiver HTML + Static Serving

- [ ] **T-1**: Create `static/cast-receiver.html` with Cast CAF SDK + Shaka Player
  - Load CAF receiver framework from Google CDN
  - Load Shaka Player from CDN
  - Intercept LOAD message, extract customData (introUrl, seekRestartUrl, etc.)
  - Basic video playback via Shaka Player
  - ~100 lines

- [ ] **T-2**: Add axum routes to serve static files
  - `GET /cast-receiver.html` → serve `static/cast-receiver.html`
  - `GET /cast-receiver/intro.mp4` → serve `~/.config/spela/intro.mp4`
  - `GET /cast-receiver/subs.vtt` → serve current subtitle WebVTT from media dir
  - Depends on: T-1

- [ ] **T-3**: Update `cast.rs` to use custom app ID
  - Read `cast_app_id` from config
  - If set, launch custom app instead of `CC1AD845`
  - If empty, fall back to Default Media Receiver (backwards compatible)
  - Pass subtitle tracks, duration, metadata, customData in LOAD message
  - Depends on: T-1

### Phase 2: Intro Playback

- [ ] **T-4**: Implement intro playback in receiver
  - On LOAD: check customData.introUrl
  - If present: play intro fullscreen, no overlay/chrome
  - Pre-load main stream via Shaka in background during intro
  - 2-second CSS opacity fade to black at end of intro
  - Transition to Shaka Player when both intro ends AND stream is ready
  - Depends on: T-1

- [ ] **T-5**: Remove intro concat from ffmpeg pipeline
  - In `transcode.rs`: remove concat filter when custom receiver is active
  - In `server.rs`: skip intro_path logic when cast_app_id is set
  - Keep backwards compatible: if no custom app ID, old concat path still works
  - Depends on: T-4

### Phase 3: Side-Loaded Subtitles

- [ ] **T-6**: Serve WebVTT subtitle endpoint
  - `GET /cast-receiver/subs.vtt` serves the WebVTT file from media dir
  - spela already fetches SRT → WebVTT during play
  - Depends on: T-2

- [ ] **T-7**: Pass subtitle tracks in Cast LOAD
  - Add TextTrackInfo to the media LOAD message
  - Include language, trackContentId pointing to subs.vtt endpoint
  - Depends on: T-3, T-6

- [ ] **T-8**: Remove subtitle burn-in from ffmpeg when custom receiver active
  - In `transcode.rs`: skip `-vf subtitles=` when cast_app_id is set
  - Audio-only transcode becomes the common path (video copy + AAC)
  - Depends on: T-7

- [ ] **T-9**: Style subtitles in receiver CSS
  - White text, 70% opacity, thin black text-shadow outline
  - Rokkitt font (Google Fonts, Rockwell alternative)
  - Small size (~3vh), bottom 8% of screen, no background box
  - Depends on: T-1

- [ ] **T-10**: Add `spela subs` CLI command
  - `spela subs off` / `spela subs en` / `spela subs sv`
  - Sends Cast protocol EDIT_TRACKS_INFO message via rust_cast
  - Depends on: T-7

### Phase 4: Seeking

- [ ] **T-11**: Add `/api/seek-restart` endpoint
  - POST `{t: <seconds>}` → kill ffmpeg, restart with `-ss <seconds>`
  - Wait for 5MB pre-buffer, return new stream URL
  - Debounce: if called while already restarting, cancel previous
  - Depends on: T-2

- [ ] **T-12**: Implement seek logic in receiver
  - On seek event: check if target is within Shaka's buffered range
  - If within: `player.seek(target)` (instant, client-side)
  - If beyond: call /api/seek-restart, show dimmed frame + spinner
  - On restart response: load new stream URL, resume playback
  - Depends on: T-1, T-11

- [ ] **T-13**: Seek-restart buffering UI in receiver
  - Dim current frame: CSS filter brightness(0.3)
  - Spinner icon on right side of seek bar
  - Seek bar remains visible and interactive during restart
  - Depends on: T-12

### Phase 5: Resume Position

- [ ] **T-14**: Add `/api/position` endpoint (save + load)
  - POST `{imdb_id, t}` → save to state.json resume_positions map
  - GET `?imdb_id=X` → return saved position
  - Depends on: T-2

- [ ] **T-15**: Receiver reports position every 30 seconds
  - `setInterval` → POST /api/position with current playback time
  - Also report on pause and on stream end (position = 0 for completed)
  - Depends on: T-1, T-14

- [ ] **T-16**: Server uses saved position in play command
  - On `do_play`: check /api/position for IMDB ID
  - If position exists and > 60s from end: pass seek_to in Cast LOAD
  - Receiver picks up seek_to from customData and seeks after stream loads
  - Depends on: T-14, T-15

### Phase 6: Overlay & Polish

- [ ] **T-17**: Netflix-style pause/seek overlay
  - Title at top (Rokkitt font, white, gradient background)
  - Seek bar at bottom (thin white fill on dark track, elapsed + total time)
  - Semi-transparent gradient overlays top 20% and bottom 20%
  - Auto-hide after 5 seconds of no interaction
  - Depends on: T-1

- [ ] **T-18**: Stream failure recovery
  - Detect Shaka Player error events
  - Show centered message: "Stream interrupted, retrying in Xs..." with countdown
  - After 10 seconds: call spela API to retry with next result
  - Resume from approximate position
  - Depends on: T-1, T-11

### Phase 7: Testing

- [ ] **T-19**: Unit tests for new server endpoints
  - /api/seek-restart parameter validation
  - /api/position save/load roundtrip
  - Subtitle track construction in Cast LOAD
  - Depends on: T-11, T-14

- [ ] **T-20**: Integration test on Chromecast
  - Play → verify intro → verify movie starts → pause → seek within buffer → seek beyond buffer → resume from position → subtitle toggle
  - Test on both Fredriks TV and Vardagsrum
  - Depends on: all above + Cast SDK registration

---

## Verification Checklist

Before marking complete:
- [ ] Intro plays fullscreen, no overlay, fades to black
- [ ] Movie plays with seek bar showing full duration
- [ ] Seek within buffer is instant
- [ ] Seek beyond buffer shows dimmed frame + spinner, then resumes
- [ ] Pause shows Netflix-style overlay (title + seek bar + gradient)
- [ ] Subtitles render correctly (white, semi-transparent, Rokkitt, no box)
- [ ] `spela subs off/en` toggles subtitles
- [ ] Resume position works across sessions
- [ ] Stream failure shows message + auto-retries after 10s
- [ ] Falls back to Default Media Receiver when cast_app_id is empty
- [ ] 38+ existing tests still pass
- [ ] Works on both Chromecast devices

---

## Notes
- **Speed vs correctness**: Core correct (seeking, intro, subs), edges fast (resume, failure recovery)
- **Backwards compatible**: Empty cast_app_id = old behavior (Default Media Receiver + ffmpeg intro/subs)
- **Blocker**: Cast SDK registration ($5) needed before T-20 (integration test). All other tasks are buildable without it
- **Font fallback**: Rokkitt from Google Fonts. If Chromecast can't load external fonts, fall back to system serif
