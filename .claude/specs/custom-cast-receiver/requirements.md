# Requirements: Custom Cast Receiver for spela

## Overview
- **Type**: Integration (replaces Default Media Receiver with custom receiver)
- **Problem**: Default Media Receiver (CC1AD845) cannot seek in fMP4 streams, shows ugly overlay during intro, burns subtitles into video requiring NVENC for every stream
- **Pain**: Friction — seeking resets to start, progress bar over intro, no subtitle switching
- **Success unlocks**: Complete media remote experience (play/pause/seek/resume/subs) + simpler transcode pipeline

---

## User Stories

### US-1: Seeking and Rewinding
**As a** spela user watching a movie on Chromecast
**I want** to seek forward/backward to any position
**So that** I can skip slow parts or rewatch scenes

**Acceptance Criteria:**
- [ ] AC-1.1: Seeking within already-transcoded content is instant (no restart)
- [ ] AC-1.2: Seeking beyond transcoded position triggers server-side restart from new position
- [ ] AC-1.3: During seek-restart, TV shows dimmed last frame + spinner on seek bar
- [ ] AC-1.4: Seek bar shows full movie duration (from source metadata)
- [ ] AC-1.5: Google Home app seek controls work

**EARS Constraints:**
- **When** user seeks within buffered range, **the system shall** seek client-side without server restart
- **When** user seeks beyond buffered range, **the system shall** request server-side restart via spela API and show buffering state
- **While** seek-restart is in progress, **the system shall** display dimmed last frame with spinner

### US-2: Intro Clip Without UI Overlay
**As a** spela user
**I want** to see my custom intro clip without the Chromecast progress bar
**So that** it feels like a real streaming service

**Acceptance Criteria:**
- [ ] AC-2.1: Intro plays fullscreen with no overlay/chrome/seek bar
- [ ] AC-2.2: Intro plays immediately (loaded by receiver locally, not from transcode stream)
- [ ] AC-2.3: Intro fades to black over ~2 seconds before movie starts
- [ ] AC-2.4: Main stream buffers in background during intro playback
- [ ] AC-2.5: If main stream is ready before intro ends, seamless transition after fade

### US-3: Side-Loaded Subtitles
**As a** spela user
**I want** subtitles as a toggleable track (not burned into video)
**So that** I can switch languages or turn them off without restarting the stream

**Acceptance Criteria:**
- [ ] AC-3.1: Subtitles served as WebVTT side-loaded track via Cast protocol
- [ ] AC-3.2: Google Home app shows subtitle toggle/picker automatically
- [ ] AC-3.3: `spela subs off` / `spela subs en` / `spela subs sv` work via CLI
- [ ] AC-3.4: Subtitle style: white text ~70% opacity, thin black outline, no background box, Rockwell font, small size
- [ ] AC-3.5: Subtitle switching is instant (no transcode restart)

### US-4: Resume from Last Position
**As a** spela user returning to a movie I paused yesterday
**I want** playback to resume from where I left off
**So that** I don't have to manually find my place

**Acceptance Criteria:**
- [ ] AC-4.1: Receiver reports current position to spela API every 30 seconds
- [ ] AC-4.2: `spela play` defaults to last saved position for same content (by IMDB ID)
- [ ] AC-4.3: User can override by seeking to 0 (start from beginning)
- [ ] AC-4.4: Position saved in spela state (survives server restart)

### US-5: Rich Pause/Seek Overlay
**As a** spela user
**I want** a Netflix-style overlay when pausing or seeking
**So that** I see what I'm watching and where I am

**Acceptance Criteria:**
- [ ] AC-5.1: Pause overlay: movie title at top, seek bar at bottom, semi-transparent gradient
- [ ] AC-5.2: Seek bar shows elapsed time + total duration
- [ ] AC-5.3: Overlay auto-hides after 5 seconds of inactivity during playback
- [ ] AC-5.4: Movie metadata (title, year) passed via Cast LOAD command

### US-6: Stream Failure Recovery
**As a** spela user watching a movie when the torrent dies
**I want** the system to show a brief message and auto-retry
**So that** my viewing isn't permanently interrupted

**Acceptance Criteria:**
- [ ] AC-6.1: On stream failure, show message on screen ("Stream interrupted, retrying...")
- [ ] AC-6.2: Auto-retry with next torrent result after 10 seconds
- [ ] AC-6.3: If retry succeeds, resume from approximate position

---

## Non-Functional Requirements

### Performance
- Intro playback: instant (local file, no server dependency)
- Seek within buffer: < 1 second
- Seek-restart: 10-45 seconds (torrent + transcode startup)
- Position reporting: every 30 seconds (lightweight)

### Error Handling
- **Philosophy**: Fail gracefully — if seek fails, keep playing. If stream dies, show message + auto-retry
- **Anticipated misuse**: Seeking rapidly (debounce seek requests), seeking during intro (skip to movie start)

### Success Criteria
- **Gut check**: Movie night works end-to-end — search, play, intro, seek, pause, resume next day, subs toggle
- **Embarrassment criteria**: Seeking that visibly breaks (black screen, frozen), ugly default Cast chrome showing through

---

## Out of Scope (v1)
- Thumbnail previews on seek bar (Netflix-style scrubbing thumbnails)
- Multiple simultaneous streams to different Chromecasts
- HTTPS / published Cast app (HTTP + test devices for now)
- Phone companion app

---

## Risks & Assumptions
- **Assumption**: Shaka Player handles fMP4 chunked streams correctly
- **Assumption**: Cast SDK test device registration allows unlimited personal use
- **Conscious debt**: HTTP-only hosting (test devices), no seek thumbnails
- **Risk**: Rockwell font may not be available on Chromecast — need web font or fallback

---

## Open Questions
- Exact Shaka Player version / CDN URL to use
- Whether Chromecast OS includes Rockwell (likely not — need Google Fonts alternative or embedded)
