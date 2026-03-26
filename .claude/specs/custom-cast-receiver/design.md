# Design: Custom Cast Receiver for spela

## Tech Stack
- **Receiver**: HTML + JavaScript (Cast Application Framework v3 + Shaka Player)
- **Server changes**: Rust (axum endpoints in spela)
- **Hosting**: Static file served by spela's axum server at `/cast-receiver.html`
- **Font**: Rockwell via Google Fonts (or Rockwell-like fallback: Rokkitt, Arvo)

---

## Architecture Overview

```
User: "spela play 1"
    │
    ▼
spela server (axum, port 7890)
    │
    ├─── Cast LOAD command via rust_cast ───► Chromecast
    │    (custom app ID, media URL,           │
    │     title, duration, subtitle tracks)    │
    │                                          ▼
    │                              Custom Receiver HTML
    │                              (fetched from spela server)
    │                                          │
    │                                    ┌─────┴─────┐
    │                                    │           │
    │                               Shaka Player   Intro
    │                               (main stream)  (local mp4)
    │                                    │
    ├─── /stream/transcode ◄─────────────┘  (chunked HTTP, fMP4)
    ├─── /cast-receiver.html                (receiver HTML)
    ├─── /cast-receiver/intro.mp4           (intro clip)
    ├─── /cast-receiver/subs.vtt            (subtitle track)
    ├─── /api/seek-restart                  (seek beyond buffer)
    └─── /api/position                      (save/load resume position)
```

### Playback Flow

```
1. spela server starts webtorrent + ffmpeg transcode
2. Cast LOAD sent with custom app ID → Chromecast loads receiver HTML
3. Receiver fetches intro.mp4 from spela server, plays fullscreen (no chrome)
4. Meanwhile, Shaka Player pre-loads main stream URL in background
5. Intro fades to black over 2 seconds
6. Shaka Player starts main stream playback (with seek_to offset if resuming)
7. Receiver shows Netflix-style overlay briefly (title, seek bar)
8. Overlay auto-hides after 5 seconds
9. During playback: receiver reports position to /api/position every 30s
10. On pause: show overlay (title top, gradient, seek bar bottom)
11. On seek within buffer: Shaka seeks client-side (instant)
12. On seek beyond buffer: receiver calls /api/seek-restart, shows dimmed frame + spinner
13. On stream death: show message, auto-retry via /api/retry after 10s
```

---

## File Structure

```
src/
├── server.rs        # Add routes: /cast-receiver.html, /cast-receiver/*, /api/seek-restart, /api/position
├── cast.rs          # Use custom app ID from config, pass subtitle tracks + duration
├── config.rs        # cast_app_id field (already added)
├── transcode.rs     # Remove intro concat (receiver handles intro). Remove subtitle burn-in (side-loaded)
├── state.rs         # Add resume_positions: HashMap<String, f64> (imdb_id → seconds)
│
static/
├── cast-receiver.html    # The custom receiver (~150 lines HTML/JS/CSS)
│
# On server (not in repo):
~/.config/spela/intro.mp4    # User's custom intro clip
```

---

## API Design

### New Endpoints

| Method | Path | Description | Request | Response |
|--------|------|-------------|---------|----------|
| GET | `/cast-receiver.html` | Serve receiver HTML | — | HTML |
| GET | `/cast-receiver/intro.mp4` | Serve intro clip | — | video/mp4 |
| GET | `/cast-receiver/subs.vtt` | Serve current subtitle file | — | text/vtt |
| POST | `/api/seek-restart` | Restart transcode from position | `{"t": 1800}` | `{"status": "restarting", "stream_url": "..."}` |
| POST | `/api/position` | Save resume position | `{"imdb_id": "tt123", "t": 2847}` | `{"status": "saved"}` |
| GET | `/api/position?imdb_id=tt123` | Get resume position | — | `{"t": 2847}` |

### Modified Endpoints

| Endpoint | Change |
|----------|--------|
| `/play` | Pass subtitle WebVTT URL + duration in Cast LOAD. Remove intro concat from ffmpeg. Use custom app ID |
| `/stream/transcode` | No longer burns subtitles (simpler ffmpeg pipeline) |

### Cast LOAD Message

```javascript
{
  contentId: "http://192.168.4.1:7890/stream/transcode",
  contentType: "video/mp4",
  streamType: "BUFFERED",  // Custom receiver handles seeking
  duration: 6907,           // From ffprobe
  metadata: {
    title: "28 Years Later",
    releaseDate: "2025",
    images: [{ url: "https://image.tmdb.org/t/p/w500/..." }]
  },
  tracks: [{
    trackId: 1,
    type: "TEXT",
    trackContentId: "http://192.168.4.1:7890/cast-receiver/subs.vtt",
    trackContentType: "text/vtt",
    subtype: "SUBTITLES",
    name: "English",
    language: "en"
  }],
  customData: {
    introUrl: "http://192.168.4.1:7890/cast-receiver/intro.mp4",
    seekRestartUrl: "http://192.168.4.1:7890/api/seek-restart",
    positionUrl: "http://192.168.4.1:7890/api/position",
    imdbId: "tt10548174"
  }
}
```

---

## Receiver HTML Design

### Visual Specifications

**Intro phase (0-5s):**
- Fullscreen video, no overlay, no chrome
- Last 2 seconds: CSS opacity transition to black

**Playback overlay (on pause/seek/start):**
- Top: movie title (Rockwell/Rokkitt font, white, semi-transparent gradient from top)
- Bottom: seek bar (thin, white fill on dark track) + elapsed/total time
- Background: semi-transparent black gradient (top 20%, bottom 20%)
- Auto-hide after 5 seconds of inactivity

**Seek-restart buffering:**
- Dim current frame (CSS filter: brightness(0.3))
- Spinner on right side of seek bar
- Seek bar still visible and interactive

**Subtitles:**
- White text, 70% opacity
- Thin black outline (text-shadow)
- No background box
- Rockwell/Rokkitt font, small size (~3vh)
- Position: bottom 8% of screen

**Stream failure:**
- Centered text: "Stream interrupted, retrying in Xs..."
- Countdown from 10
- Semi-transparent dark overlay

---

## Seek-Restart Protocol

```
1. User drags seek bar to 1:15:00 (beyond transcoded position)
2. Receiver detects: requested_time > buffered_end
3. Receiver sends POST /api/seek-restart {t: 4500}
4. spela server:
   a. Kills current ffmpeg process
   b. Starts new ffmpeg with -ss 4500 (input seeking, fast)
   c. Waits for 5MB pre-buffer
   d. Returns {status: "ready", stream_url: "/stream/transcode?restart=1"}
5. Receiver loads new stream URL into Shaka Player
6. Playback resumes from 1:15:00
```

**Edge case: seek during seek-restart** — debounce. If user seeks again while restart is in progress, cancel the pending restart and start a new one.

---

## Transcode Pipeline Simplification

### Before (current)
```
webtorrent → ffmpeg (concat intro + burn subs + transcode audio/video) → /stream/transcode → Chromecast
```

### After (with custom receiver)
```
webtorrent → ffmpeg (transcode audio/video only) → /stream/transcode → Shaka Player
                                                                         ↑
intro.mp4 (served directly) ──────────────────────────────────────► Receiver plays locally
subs.vtt (served directly) ────────────────────────────────────────► Cast subtitle track
```

**Impact**: Most streams (H.264 + AC3/DTS) only need audio transcode (video copy). No NVENC. Startup drops from ~30s to ~10s. Intro plays instantly during that buffer time.

---

## Trade-off Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| HTTP vs HTTPS | HTTP (test devices) | Personal use, iteration speed. HTTPS later |
| Shaka vs HTML5 video | Shaka Player | Handles fMP4 seeking, adaptive buffering, better error recovery |
| Intro in receiver vs ffmpeg | Receiver | Eliminates NVENC for intro, instant playback, simpler pipeline |
| Subs side-loaded vs burned | Side-loaded WebVTT | Instant switching, no transcode restart, Cast protocol native |
| Seek: client vs server | Hybrid | Client for buffered range, server restart beyond |
| Font: Rockwell | Rokkitt (Google Fonts) | Rockwell not on Chromecast. Rokkitt is closest free match |

---

## Security Considerations
- Receiver HTML served on LAN only (port 7890, same as API)
- No authentication on receiver endpoints (same trust model as existing API)
- Position data stored locally (state.json), not sent externally
