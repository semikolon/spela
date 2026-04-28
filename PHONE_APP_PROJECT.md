# Phone App — Project Checkpoint

Status: **brainstorming / scoping**, picked up on Mac Mini for continuation.
Date: Apr 28, 2026.

## Goal (verbatim from user)

> "What's missing / required for me to have a phone app that's like the Netflix or HBO apps but I can play anything through spela? Either directly on my phone or choose to cast it to my Chromecast if I'm home?"

## User preferences / context (verbatim)

- Distribution: native iOS. Sharing scope:
  > "I might share this with roomies, friends and family, but the vast majority of them all have iPhones"
- Dev effort:
  > "My dev velocity these days is insane so dev effort should not be a factor here basically"
- Remote access via existing WireGuard:
  > "I've got WireGuard set up on Darwin (home router and server) so we should use that instead of Tailscale I guess?"
- WG split-tunnel preference:
  > "ideally (minor issue tho) I'd like it if ONLY the spela app traffic was tunneled through WG through the Sarpetorp network. But I guess it doesn't matter that much."
- On running spela on the phone itself:
  > "Could/should the phone app run spela on the phone itself to stream directly from the torrent? Or is that a bad idea?" — confirmed bad idea (battery, cellular caps, mobile-IP exposure, iOS background-kill).
- On reusing the Custom Receiver on the phone:
  > "It wouldn't be possible to use the planned new media receiver app for this? That's just for the Chromecast side I guess?" — confirmed Custom Receiver is purely Chromecast-side.
- On the $99/yr Apple Developer fee:
  > "Sounds like a hassle without it then. :/"

## Decisions made

1. **Native iOS app** (SwiftUI). No Android v1.
2. **Centralized spela on Darwin**, phone is a thin client. No torrent/transcode on phone.
3. **WireGuard for remote access** (not Tailscale). Existing WG terminates on Darwin.
4. **Use the official WireGuard iOS app**, not bake WG into the spela app. Set `AllowedIPs` to Darwin's LAN subnet only (e.g. `192.168.4.0/24`) so only spela traffic tunnels.
5. **Custom Cast Receiver remains TV-side only.** Reuse Shaka logic at most as library/pattern, not as artifact.

## Open questions (in dependency order)

### Tier 1 — gates everything

- [ ] **Apple Developer Program $99/yr — pay it?** Required in practice for TestFlight (the only non-painful way to share a sideloaded app with roomies/friends/family). Free path means weekly re-signs and reduced entitlements.
- [ ] **Phone-direct playback importance.** Is the phone primarily a *remote* for the TV, or do you actually want to watch on the phone (couch, bed, travel) regularly? Determines whether a phone transcode profile, soft subs, and offline download are worth building.

### Tier 2

- [ ] **Browse depth.** Minimal (search box + continue watching) / medium (+ trending row + new episodes) / full Netflix-shape (genre browse, recommendations, hero carousel)?
- [ ] **Cast strategy.** Pay the $5 Cast SDK gate + integrate iOS Cast SDK (standard cast button, eventual Custom Receiver native seeking) **OR** custom "Send to TV" picker that routes through spela's existing `/play --cast`?

### Tier 3 — refinements

- [ ] **Subtitles.** Soft subs with language toggle (proper UX, more transcode work) or burned-in for v1?
- [ ] **Offline download.** Ship "download for the plane" feature or skip?
- [ ] **Default behaviors.** Auto-play next episode? Skip intro? Auto-resume always or prompt?
- [ ] **Search UX.** Auto-play top ranker pick (current CLI behavior) or show a picker with all results?
- [ ] **WG activation.** Always-on, or auto-connect when the spela app is foregrounded (iOS on-demand rules)?
- [ ] **Multi-stream concurrency.** Need phone+TV simultaneously, or is single-active-stream fine? (Real backend refactor if multi.)
- [ ] **Multi-user state.** Per-user resume positions/watchlists, or single user across the household?

---

## Reference

### What spela already gives you for free
- HLS endpoint (`/hls/master.m3u8`) plays in iOS native `AVPlayer` / Safari `<video>` directly.
- Search → play pipeline works as-is (TMDB + Torrentio + 5-tier ranker + dead-seed retry).
- Smart Resume keyed per-episode, server-side.
- Subtitle fetch + shift server-side.
- Cast routing via `/play --cast` already works.
- Self-healing (dead seeds, IDLE auto-recast, reaper).

### Backend gaps that need filling
- Browse endpoints (TMDB wrappers: trending, genre, show details, episode list).
- Poster URLs surfaced in `SearchResult` (`tmdb_id` is there, URL isn't).
- Watchlist CRUD + library/history endpoints.
- SSE/WebSocket status stream (replaces polling).
- Phone transcode profile (720p ~3 Mbps) — or ABR multi-rendition ladder.
- Soft subtitles via WebVTT sidecar in master playlist.
- Auth — likely none, rely on WG tunnel as the security boundary.

### Aspects checklist (full surface area)

**Frontend**
- Player engine: native `AVPlayer` (likely) vs hls.js vs Shaka.
- Browse UI shape.
- Search UX (as-you-type, filters, multi-result vs auto-top).
- Episode picker + auto-play-next.
- Player controls: subs toggle, lang select, skip-intro, resume banner, PiP, background audio.
- Cast button: native Cast SDK vs custom "send to TV".
- Settings: default cast target, default sub lang, skip-intro toggle, auto-play-next toggle.
- Error/loading UX (cold-start ~60s, "spela unreachable").

**Backend additions**
- Browse endpoints (TMDB wrappers).
- Poster URLs in `SearchResult`.
- Watchlist CRUD + library/history endpoints.
- SSE/WebSocket status stream.
- Phone transcode profile.
- Soft subs via WebVTT sidecar.
- Auth surface (or none).
- Multi-user state.

**Architecture / structural**
- Single-stream vs multi-stream concurrency.
- Single-user vs multi-user.
- Offline download for travel.
- Multi-device handoff ("continue on TV").

**Infra**
- WG `AllowedIPs` scoping (decided: Darwin LAN subnet only).
- WG always-on vs on-demand by SSID.
- Cast SDK $5 gate (defer or pay).
- App Store / sideload / PWA distribution path (decided: TestFlight via paid Apple Dev, pending Tier 1 confirmation).

### Dependency map

```
[Distribution] ─────┬──► Player engine
   (DECIDED: iOS)   ├──► Cast strategy ──► $5 gate
                    └──► Auth surface

[Solo vs household] ─┬──► Concurrency model ──► state.rs/server.rs refactor scope
                     └──► Multi-user ──► Auth ──► State schema

[Phone-direct vs cast-remote] ─┬──► Transcode profile (phone needs 720p)
   (Tier 1 OPEN)                ├──► Soft subs needed?
                                └──► Player engine importance

[Browse depth] ──► Backend endpoint count + TMDB wrapping work

[Cast strategy] ─┬──► Cast device discovery (SDK mDNS vs spela /targets)
                 └──► Cast button UI complexity

[Offline download] ──► State schema + disk policy + UI mode switch
```

### Native iOS upsides (non-effort)
- `AVPlayer` (gold-standard HLS, hardware-decoded, free PiP + AirPlay).
- System media integration (lock-screen controls, Now Playing, Dynamic Island showing currently casting).
- Siri Shortcuts ("Hey Siri, play The Boys").
- Home-screen widgets ("continue watching").
- Native iOS Cast SDK button, polished mini-player + expanded controller.
- 120Hz ProMotion scroll on poster grid.
- PWAs are second-class on iOS (Safari evicts caches, no MediaSession). Native sidesteps this slow-grinding tax.

### Native iOS downsides (non-effort)
- $99/yr Apple Developer Program + 90-day TestFlight rebuilds (real ongoing tax).
- iOS-only — locks out Android users in the household (acceptable here per user).
- Two player implementations to maintain (Swift on phone, JS on TV Custom Receiver) with no code sharing.
- `AVPlayer` is strict about HLS manifests — spela's hand-rolled `master.m3u8` may need tweaks.

### Distribution paths considered
- **App Store**: rejected (Apple won't approve torrent app).
- **TestFlight**: $99/yr, 90-day rebuilds, friends install via link. **Likely path.**
- **Ad Hoc**: $99/yr, 1-year profiles, but each friend submits UDID, max 100 devices per device class per year.
- **Free Apple ID + Xcode signing**: 7-day re-sign treadmill, reduced entitlements, hard to share.
- **AltStore (worldwide)**: free, but each friend installs AltServer on their computer and re-signs over Wi-Fi. Fragile.
- **AltStore PAL (EU/Sweden, DMA)**: publisher still needs Apple Dev account for notarization, doesn't actually skip the $99.

### Glossary
- **Sideloading**: installing an app via any route other than the official store. On iOS: Xcode/TestFlight/AltStore/ad-hoc. On Android: APK install. Term comes from "side" vs the front-door store.
- **Sender / Receiver / Source** (Cast terminology):
  - Sender = the phone/Chrome app that initiates and controls the cast session.
  - Receiver = HTML5 app running *on the Chromecast device itself* (Default Media Receiver or our Custom Receiver). Cannot run on a phone.
  - Source = where the media bytes come from (spela's HLS endpoint).

## Where to pick up

Answer Tier 1 first:
1. Pay $99/yr Apple Developer or use free path?
2. Phone-direct playback important, or phone primarily a TV remote?

Most of Tier 2 and Tier 3 fall out of those two answers.
