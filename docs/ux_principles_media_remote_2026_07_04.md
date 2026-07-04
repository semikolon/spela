# UX Principles for a Cast-Remote Media SPA (2026 Research)

**Scope**: A single-page web "remote" that controls a torrent-to-Chromecast media server. The user searches for video, taps play, waits for a stream to *start on a Chromecast/TV*, then controls play/pause/stop/seek — all of which happen on a **remote device** reachable only over a network round-trip. This document synthesizes current (2024–2026) authoritative UX guidance into concrete, actionable principles for exactly this shape of app.

**Research date**: 2026-07-04. Every claim is cited inline with its source URL.

---

## 0. The one-paragraph summary

For a cast remote, the enemy is **round-trip latency between the remote UI and the device that actually plays**. The fix is a layered latency strategy: (1) give **local visual feedback within ~100 ms** of every tap so the button *feels* instant (source: Nielsen; Doherty Threshold); (2) **optimistically reflect the intended state immediately** (pause shows paused, stop clears the now-playing) and reconcile against the device when confirmation arrives, rolling back visibly on failure; (3) for the one genuinely slow operation — **stream start on the TV** — show a minimal indicator by default and escalate to detailed status/errors only when a time budget is exceeded; (4) keep the remote and device state **in sync via polling/push** because the device can be controlled by *other* remotes and its own controls; (5) never let a critical failure live only in a transient toast — keep failure + recovery attached to the now-playing surface.

---

## 1. Action feedback & perceived responsiveness — the canonical latency numbers

### Nielsen's 3 response-time limits (the foundational law)

These come from 40+ year-old human-factors research and have been re-confirmed by NN/g as unchanged today ([nngroup.com/articles/response-times-3-important-limits](https://www.nngroup.com/articles/response-times-3-important-limits/); [nngroup.com/articles/website-response-times](https://www.nngroup.com/articles/website-response-times/)):

| Limit | Perception | Design requirement |
|---|---|---|
| **0.1 s (100 ms)** | Feels **instantaneous** — the outcome feels *caused by the user*, not the computer ("direct manipulation"). | No special feedback needed *except* to display the result. This is the target for tap/press states, toggles, selection highlights. |
| **1 s** | User notices a delay but **keeps their flow of thought**; still feels in control. | Below 0.1s isn't achievable → aim under 1s. Beyond 1s, indicate the system is working (e.g. change cursor / show a busy state). |
| **10 s** | **Limit of holding attention.** After ~10s the mind wanders; users start doing other things and struggle to re-orient. | Anything slower needs a **percent-done indicator** *and* a clearly signposted **way to cancel/interrupt**. Assume the user must re-orient on return. |

Direct quotes worth internalizing (from the same NN/g articles):
- *"0.1 second gives the feeling of instantaneous response — that is, the outcome feels like it was caused by the user, not the computer."*
- *"1 second keeps the user's flow of thought seamless."*
- *"After 10 seconds, they start thinking about other things, making it harder to get their brains back on track."*

NN/g's "Powers of 10" framing reinforces this: to create *the illusion of direct manipulation*, the UI must respond **faster than 0.1 second** ([nngroup.com/articles/powers-of-10-time-scales-in-ux](https://www.nngroup.com/articles/powers-of-10-time-scales-in-ux/)).

### The Doherty Threshold (~400 ms — the productivity/flow number)

Walter Doherty & Arvind Thadhani (IBM, 1982), popularized by *Laws of UX* ([oreilly.com/library/view/laws-of-ux/9781098146955/ch10.html](https://www.oreilly.com/library/view/laws-of-ux/9781098146955/ch10.html); [uxgenstudio.com/ux-laws/the-doherty-threshold](https://uxgenstudio.com/ux-laws/the-doherty-threshold/)):

> *"Productivity soars when a computer and its users interact at a pace (<400 ms) that ensures that neither has to wait on the other."*

The original IBM data is striking: dropping system response from 3 s → 0.3 s **doubled** programmer transactions/hour (180 → 371) — *"a reduction of 2.7 seconds saves 10.3 seconds of the user's time"* (the empirical basis, [archive.computerhistory.org PDF](https://archive.computerhistory.org/resources/access/text/2024/03/102751398-05-01-acc.pdf)).

**How the Doherty Threshold reconciles with Nielsen's 100 ms** — from UXGen Studio's practitioner guidance:
> *"Even when the back-end requires longer processing, return lightweight UI feedback within ~100–200 ms (press state, skeleton, optimistic UI). This keeps you under the Doherty threshold for perceived response, even if final data lands slightly later."*

So the operating rule is: **the *acknowledgment* must land in ~100–400 ms; the *result* can land later behind an indicator.** UXGen also ties this to Google's modern **INP (Interaction to Next Paint) ≤ 200 ms** target — meet INP and you naturally meet Doherty.

### The tap-feedback trap on touch devices (the ~300 ms delay you must kill)

Mobile browsers historically add a **~300 ms delay before `:active` fires**, because they wait to see whether the tap is a double-tap-to-zoom ([stackoverflow.com/questions/71676756](https://stackoverflow.com/questions/71676756/how-to-remove-the-delay-until-the-active-class-is-added-to-a-button-on-mobile)). This silently *breaks* the 100 ms rule on a phone remote. Fixes:
- **`touch-action: manipulation;`** on interactive elements — disables double-tap-zoom and removes the delay (the standard, CSS-only fix).
- Or a `touchstart`-driven active class (`quicktap`-style) for instant press feedback ([github.com/marcoms/quicktap](https://github.com/marcoms/quicktap)).

The perceptual latency ceiling for *visual* touch feedback (research-grade): **feedback should land within ~30–85 ms of the finger touch** to be perceived as simultaneous and not degrade perceived quality ([ACM TAP, dl.acm.org/doi/10.1145/2611387](https://dl.acm.org/doi/10.1145/2611387)). Tactile 5–50 ms, audio 20–70 ms, visual 30–85 ms.

### Applied to the cast remote

- Every transport button (▶ ⏸ ⏹ ⏮ ⏭ ↺, scrubber grab) MUST paint a `:active`/pressed state in **<100 ms, locally, before any network call**. Never let the button's visual response wait on the Chromecast round-trip.
- Add `touch-action: manipulation` to every control (spela's web remote already does this per its override-block — keep it).
- The *result* of the action (device actually paused) can arrive up to ~1 s later; that gap is bridged by optimistic UI (§2), not by a spinner on the button.

---

## 2. Optimistic UI — reflect the action immediately, reconcile against the device, roll back on failure

This is the single most important pattern for a cast remote, because the "server" is a TV several network hops away.

### The core pattern

> *"Optimistic UI updates the UI immediately, assuming the server operation will succeed, and if it later fails, you roll back the UI to the correct state… The user perceives the UI update as instant — but the actual operation may take place in the background."* ([freecodecamp.org/news/how-to-use-the-optimistic-ui-pattern-with-the-useoptimistic-hook-in-react](https://www.freecodecamp.org/news/how-to-use-the-optimistic-ui-pattern-with-the-useoptimistic-hook-in-react/))

Three things a production-grade optimistic update needs ([matheuspalma.com/blog/optimistic-ui-server-reconciliation-patterns](https://matheuspalma.com/blog/optimistic-ui-server-reconciliation-patterns)):
1. **A reversible local patch** — you can roll back or replace provisional state.
2. **A stable identity for the operation** — retries and duplicate responses don't create duplicate effects.
3. **A merge rule when truth arrives** — the server (device) response reconciles with whatever else happened on the client.

Palma's five-state machine is the right mental model for each control action:
- **Idle** — no in-flight mutation.
- **Pending (optimistic)** — local state reflects the *intended* outcome; a request is in flight.
- **Committed** — device confirmed; local state matches canonical.
- **Failed** — device rejected or transport failed; local state must be corrected.
- **Superseded** — a newer action or a refetch replaced this operation's view (critical here — see §4, multi-controller).

### Spotify Connect is the textbook precedent for *your exact problem*

Spotify Connect's "Triangle" architecture (Controller → Cloud → Receiver) faces the identical remote-controls-a-remote-device latency, and solves it optimistically ([medium.com/@rushichavan2327/the-triangle-architecture](https://medium.com/@rushichavan2327/the-triangle-architecture-a-deep-dive-into-spotify-connects-engineering-6c7d21f9f2a3)):

> *"When you slide the volume bar on your phone, the app UI updates immediately — before it receives confirmation from the server that the command worked. The app 'optimistically' assumes the network request will succeed. By the time the actual signal round-trips (usually in under 200 ms), your brain has already accepted the visual feedback, making the experience feel synchronous."*

Perceived-input-to-paint targets from the optimistic-UI literature: **<100 ms** is the envelope; best-in-class (Linear, Figma) target **<50 ms** ([cadence.withremote.ai/blog/optimistic-ui-react](https://cadence.withremote.ai/blog/optimistic-ui-react)).

### When to reflect immediately vs. wait for confirmation

| Action | Reflect optimistically? | Why |
|---|---|---|
| **Pause / Resume** | **Yes, immediately.** | Fully reversible, cheap round-trip, high-frequency. Flip the button glyph on tap; reconcile from the next device status poll. |
| **Stop / Disconnect** | **Yes, immediately** (clear now-playing) — but see §5 for the reconciliation nuance. | User expects instant "it stopped." |
| **Seek (in-flight)** | **Yes** — move the scrubber thumb to the released position immediately; let the device catch up. | Direct-manipulation expectation; the thumb IS the feedback. |
| **Volume** | **Yes** (per Spotify). | Reversible, high-frequency. |
| **Play/Start-a-new-stream** | **Partially** — reflect "starting" state instantly, but this is the ONE genuinely slow op that needs a real loading state (§3). | Torrent + transcode + cast handshake can take 10–60 s; you cannot honestly show "playing" yet. |

### Reconciliation & rollback rules

- **On success**: do nothing if you already showed the optimistic state; if you keep a separate "confirmed" copy, replace it. If the device returns full state, replace the optimistic slice entirely ([matheuspalma.com](https://matheuspalma.com/blog/optimistic-ui-server-reconciliation-patterns)).
- **On failure**: transition to **Failed** and **correct the UI** — *"No rollback path — The UI keeps optimistic data after 4xx/5xx. Always transition to failed or refetched state."* This is called out as the #1 optimistic-UI bug.
- **Roll back *visibly*, not with a jarring snap.** Best practice from Linear's implementation ([cadence.withremote.ai](https://cadence.withremote.ai/blog/optimistic-ui-react)):
  - A short animation back to prior state (**200–300 ms ease-out**), not an instant snap.
  - A toast that *names the action and offers retry*: **"Could not send. Tap to retry."** — not "Error 500."
  - A persistent error indicator on the affected control until acknowledged/retried.
- **Handle "Superseded"**: if the user taps pause then quickly resume, or another controller changes state mid-flight, your reducer must fold the *latest* device truth in, not an outdated snapshot (React's `useOptimistic` re-runs the reducer against the new base state for exactly this reason — [react.dev/reference/react/useOptimistic](https://react.dev/reference/react/useOptimistic)). For a poll-driven SPA, the rule is: **an in-flight optimistic action should not be clobbered by a stale poll, and a newer poll/action supersedes an older optimistic patch.**

> **Cast-remote-specific caveat**: because the device can be controlled by *other* senders and its own remote (§4), your reconciliation source-of-truth is the **device status**, not your own last request. Optimism bridges the gap; the periodic device-state read is the authority. Never let the poll *re-assert* an intent it shouldn't — spela's own hard-won lesson: the now-playing `playing` flag is **user-intent-only**, never reconciled from `/status` polling, or every tap re-sends pause and resume becomes unreachable (see spela CLAUDE.md "Web-remote now-playing state machine").

---

## 3. Loading & progress states — minimal by default, detail on anomaly

### The pattern-to-duration mapping (NN/g canonical)

From NN/g's "Skeleton Screens vs. Progress Bars vs. Spinners" and "Progress Indicators" ([nngroup.com/articles/skeleton-screens](https://www.nngroup.com/articles/skeleton-screens/); [nngroup.com/articles/progress-indicators](https://www.nngroup.com/articles/progress-indicators/)):

| Wait duration | Right indicator | Notes |
|---|---|---|
| **< ~0.3–1 s** | **Nothing** (or just the instant press state). | A spinner for a sub-second op is *distracting* — "users cannot keep up with what happened and might feel anxious about whatever flashed." Modern practitioner consensus: **only show a loading state above ~300 ms** ([blog.vibecoder.me/skeleton-screens-loading-indicators-patterns](https://blog.vibecoder.me/skeleton-screens-loading-indicators-patterns)). |
| **2–10 s** | **Spinner** (single module) or **skeleton** (full page/grid). | Spinner = *"best used on a single module, like a video or a card."* Skeleton = *"better when the full screen is loading… gives users a sense of what the page will look like and minimizes cognitive load."* Skeletons feel **20–30% faster** than spinners for content loads at identical actual speed ([nngroup video](https://www.nngroup.com/videos/skeleton-screens-vs-progress-bars-vs-spinners/); [docs.specvital.com](https://docs.specvital.com/en/adr/web/20-skeleton-loading-pattern)). |
| **≥ 10 s** | **Percent-done progress bar** (or step list) **+ a cancel affordance.** | *"Progress bars are strongly recommended for any page that takes longer than 10 seconds… Anything above 10 seconds requires an explicit estimation of duration."* If you can't estimate %, show **completed/remaining steps** instead. |

Because delay is often unpredictable, NN/g's explicit advice: **lower the cutoff** for the more-detailed indicator when your time estimates are variable — *"The bigger the variability in your estimates, the lower the threshold for showing the more elaborate feedback."* Torrent-start latency is *extremely* variable → bias toward richer feedback sooner.

### Rules that matter for the "starting a stream on the TV" case

1. **Always give immediate feedback the instant Play is tapped** — *"A user's wait time begins the moment she initiates an action… Without any visual change, most users will assume the action was not registered and they will try again."* ([nngroup progress indicators](https://www.nngroup.com/articles/progress-indicators/)). Show a "Starting on [TV name]…" state immediately.
2. **Never use a bare indefinite spinner for a 10+ s op** — *"if a spinner is rotating indefinitely, users cannot be sure if the system is still working or if it's stopped, so they may decide to abandon."* A stream-start that can take 30–60 s needs progress texture, not an eternal spinner.
3. **A static "Loading…"/"Please wait" text is an anti-pattern** — *"static indicators should be replaced… If the system hangs, the user has no way of knowing they need to restart."*
4. **Never show a "don't click again" warning** — the correct answer is to show the first click was accepted (the "Starting…" state) so the user isn't tempted to re-tap.

### The "minimal by default, detail on anomaly" pattern (your exact ask)

This is the practitioner synthesis of NN/g's status-tracker guidance + progressive disclosure + context-sensitive performance budgets:

- **Default (normal path)**: show the *minimal* honest indicator — a compact "Starting on Fredriks TV…" with a spinner/indeterminate bar, and (once known) a lightweight percent as the torrent buffers. Keep it calm; don't surface plumbing.
- **On anomaly (exceeded time budget or partial failure)**: *progressively disclose* detail. Progressive disclosure = *"Initially, show users only a few of the most important options. Offer a larger set… only if a user asks for them"* ([nngroup.com/articles/progressive-disclosure](https://www.nngroup.com/articles/progressive-disclosure/)). Apply it to *status verbosity*: reveal per-step status ("Finding peers → Downloading N% → Transcoding → Casting"), seed/peer health, or a "still working, this source is slow" note **only when a time budget is crossed** or a retry fires.
- **Status text must be plain-language, not backend jargon** — NN/g status-tracker guideline #3: *"Backend codes and internal jargon, such as 'fulfilled' or 'label created', mean nothing to the user."* ([nngroup.com/articles/status-tracker-progress-update](https://www.nngroup.com/articles/status-tracker-progress-update/)). So surface "Finding a fast source…" not "0/57 seeds, probe timeout."
- **Long processes need *regular* updates even at low granularity** — guideline #9: *"When updates are few and far between, status trackers lose their value… users start to think perhaps something went wrong."* For a slow torrent start, a periodically-ticking "Downloading 34%… 41%…" beats a frozen spinner even if you can't give an ETA. For complex-app waits >10 s, NN/g explicitly recommends communicating **% done or a completed/remaining step list**, kept **highly salient and discoverable** ([nngroup.com/articles/designing-for-waits-and-interruptions](https://www.nngroup.com/articles/designing-for-waits-and-interruptions/)).
- **Perceived-performance context budget**: user tolerance is context-dependent — *"A user initiating a complex search expects to wait longer than a user clicking a navigation link."* ([atticusli.com/blog/posts/psychology-loading-states-perceived-performance](https://atticusli.com/blog/posts/psychology-loading-states-perceived-performance/)). "Start a torrent stream on a TV" is a heavyweight op the user *knows* is heavy — so a visible, well-communicated 20–40 s wait is acceptable *if* it shows progress and purpose. Spend your perceived-perf budget on the *acknowledgment* (instant) and on *progress texture*, not on trying to make the actual buffer faster.

**Concrete recommendation for spela's play-flow**: the second-by-second warmup view (download %/peers/speed → buffering-segments) noted as designed-but-not-built in spela's TODO is *exactly* the right instrument — it converts the most variable, most-abandoned wait into a percent-/step-textured status that satisfies NN/g's ≥10 s rule. Keep it minimal on the happy path (just "Starting… NN%"), and expand to peers/speed/seed-health only when the wait runs long or stalls.

---

## 4. Cast / remote-control specifics — the gap between the remote UI and the playing device

### The Google Cast interaction model (the authoritative reference)

Google's own UX guidelines define the model precisely ([developers.google.com/cast/docs/ux_guidelines](https://developers.google.com/cast/docs/ux_guidelines)):

> *"The mobile phone, tablet or laptop is the **sender** which acts as a remote control… the TV is the **receiver**… Casting relies on the coordination between two or more screens; the sender UI and the receiver UI — they must work together. For example, if you press a button on a mobile device to pause the content, the TV should indicate that it is paused, while the mobile device should provide a play button to resume playback."*

Two design principles from the same doc, load-bearing for the remote:
- **Sender supports actions; receiver displays state.** The remote issues commands; the source of truth for *what's actually happening* is the device.
- **"Speed matters. Users need to be able to… see content start playing immediately… While content is loading, provide animated loading indicators and use transitions to help make things feel faster."**

### Keep sender and receiver in sync — even for changes the sender didn't make

The Cast *sender* design checklist is explicit and this is the crux of multi-controller correctness ([developers.google.com/cast/docs/design_checklist/sender](https://developers.google.com/cast/docs/design_checklist/sender)):

> *"The sender app's Cast playback status and controls… must be in sync with playback changes happening on the Web Receiver, **even when not originated by the sender app**. This will allow proper handling of both multi-sender commands and the playback control coming from the device's remote controls, buttons, etc."*

**Implication for a polling SPA**: your now-playing state must be periodically reconciled from the *device* (or a device-proxying server endpoint), because another phone, another browser tab, or the TV's own remote can pause/stop it. This is the "Superseded" state from §2 made concrete. The device is authoritative; your optimistic patches are provisional.

Spotify Connect's SDK guidance mirrors this: functions like shuffle/repeat *"reflect the state of the playback among all Connect-enabled devices, even if the device is not the active playback device,"* and the controller must **actively listen** for playback-state notifications (`kSpPlaybackNotifyPause`/`Play`) rather than assume ([developer.spotify.com/documentation/commercial-hardware/implementation/guides/connect-basics](https://developer.spotify.com/documentation/commercial-hardware/implementation/guides/connect-basics)).

### "Now playing" + persistent mini-controller — the device keeps playing when you navigate away

This is the specific behavior you asked about. Cast's model is a **multi-tasking** one — the user browses the app while the TV keeps playing ([developers.google.com/cast/docs/design_checklist/cast-button](https://developers.google.com/cast/docs/design_checklist/cast-button)):

> *"Google Cast employs a multi-tasking model, which allows users to browse the sender app… while casting. The Cast button must be visible from every screen where there is playable content, so the user doesn't have to hunt to find where to pause or stop the content playing on TV."*

The mechanism is the **mini controller** ([developers.google.com/cast/docs/design_checklist/sender](https://developers.google.com/cast/docs/design_checklist/sender)):

> *"A small, persistent control known as the mini controller should appear, while casting, when the user navigates away from the current content page or expanded controller to another view within the sender app. The mini controller is a visible reminder of the current cast and provides instant access to it."*

And a full **expanded controller** for the dedicated now-playing view. Plex implements exactly this — a **Now Playing** screen that toggles to a **mini-player** so you can keep browsing while it plays on the Chromecast ([support.plex.tv/articles/201206866-cast-from-browser-or-desktop](https://support.plex.tv/articles/201206866-cast-from-browser-or-desktop/)).

**Design rules for the "navigate away" case**:
1. **Do NOT stop the stream** when the user leaves the now-playing view. Casting is intentionally decoupled — Plex: *"it's possible to start the cast with one device and then connect… later from a completely different device to control the playback."*
2. **Show a persistent mini-controller** on every other view (search, library grid) with at minimum: poster/title thumbnail, play/pause, and tap-to-expand-to-now-playing. This is the "visible reminder" that prevents the user from thinking the cast died.
3. **Make the control affordance reachable from everywhere** — the equivalent of Cast's "Cast button visible from every screen."

### Making stop/pause feel instant despite device latency

The synthesis of §1–§4:
- **Local press feedback < 100 ms** (button paints pressed state; §1).
- **Optimistic state flip immediately** (glyph changes to the intended next state; §2) — Spotify does this for volume in <200 ms perceived-sync.
- **Fire the command to the device**; on the next device-status poll, **reconcile** (if the device disagrees — e.g. another controller countermanded it — the poll wins and the UI corrects).
- If the command **fails**, roll back with a 200–300 ms ease and a retry toast (§2, §6).

---

## 5. Stop / disconnect semantics — what "stop" should do, and how to signal it worked

There are **two distinct meanings** and best-in-class apps separate them. Google's checklist distinguishes **"Stop Casting"** vs **"Disconnect"** ([developers.google.com/cast/docs/design_checklist/sender](https://developers.google.com/cast/docs/design_checklist/sender)):

> *"Content which is cast to a TV continues playing until either a user chooses Stop Casting or a sender casts something new. When multiple senders are connected to the same Web Receiver, each sender app should have a **Disconnect** button (instead of a Stop Casting button)…"*

| Semantic | Effect on the **device** | Effect on the **remote UI** |
|---|---|---|
| **Stop** (end playback) | Playback halts on the TV; receiver returns to idle/home. | Now-playing cleared; return to browse; mini-controller disappears. |
| **Disconnect** (stop *controlling*, keep playing) | Device keeps playing. | Remote stops showing controls / hands off; useful when handing control to another device or leaving the room. |

Plex models the disconnect-with-choice elegantly ([support.plex.tv/articles/201165566-controlling-flung-media](https://support.plex.tv/articles/201165566-controlling-flung-media/)):

> *"While controlling another Plex app, opening the Players menu will allow you to disconnect… you'll be asked whether you wish to continue playback on your controller device. Accepting means playback will stop on the [device] and resume on the controller. If you decline, playback stops on the [device] and nothing happens on the controller."*

**Recommendations for the spela remote (which today has a single-user, primarily-one-controller model)**:
- Default the primary control to **Stop** (end playback on the TV) — matches user mental model of "stop it."
- **Signal success subtly but clearly** (NN/g visibility-of-system-status heuristic): on confirmed stop, the now-playing surface should *transition out* (fade, not snap — Cast: *"transitions… should be smooth and feel cinematic… fade-in and fade-out"* [developers.google.com/cast/docs/ux_guidelines](https://developers.google.com/cast/docs/ux_guidelines)), the mini-controller disappears, and the Cast/target indicator drops to its disconnected state. That *disappearance* of the now-playing surface IS the confirmation — no loud toast needed on success.
- **Optimistic + reconcile**: reflect "stopped" instantly (clear now-playing), fire stop to the device, and confirm from the next status read. If the device *fails* to stop, restore the now-playing surface and surface a retry (§6) — do not silently leave a UI that says "stopped" while the TV keeps playing (that's the dangerous divergence spela's own attribution lessons warn about).
- If you later add multi-controller, split into **Stop** (halt on TV) vs **Disconnect** (leave it playing) per Google's guidance.

---

## 6. Error surfacing that aids debugging without alarming

The governing principle: **design errors by impact/severity**, keep the failure attached to the task, and reserve loud/technical detail for opt-in diagnostics.

### Severity-graded surfacing (NN/g)

From NN/g's Error-Message Guidelines ([nngroup.com/articles/error-message-guidelines](https://www.nngroup.com/articles/error-message-guidelines/)):

> *"Design your error messages to indicate the problem's severity… conditionally displayed labels, toast notifications, or banners can be used for issues needing minimal user interaction, whereas modal dialogs require the user's attention and resolution and should be reserved for severe errors."*

Other load-bearing NN/g rules:
- **Human-readable language; hide error codes.** *"Hide or minimize obscure error codes or abbreviations; show them for technical diagnostic purposes only."* → your seed counts, probe timeouts, ffmpeg logs, HTTP codes are **diagnostic detail**, not front-line copy.
- **Don't blame the user.** Avoid "invalid/illegal/incorrect."
- **Offer constructive advice / a next action** ("This source is slow — trying another…").
- **Don't display errors prematurely** — *"Presenting errors too early is a hostile pattern."* A stream that's merely slow is not yet an error; only surface failure after the time budget is genuinely exceeded.
- **Mitigate total failure with novelty** — for the rare catastrophic case (nothing can play), a light apology + calm framing beats a raw stack dump (peak-end rule).

### The toast anti-pattern for critical failures (keep failure + recovery attached to the task)

This is directly relevant to a "the stream failed to start" situation. Multiple sources converge:
- **Toast-only critical error is an anti-pattern** ([uxpatternsguide.com/patterns/toast-only-critical-error](https://uxpatternsguide.com/patterns/toast-only-critical-error/)): *"A blocking or high-consequence failure is announced only in a transient toast, so users can miss what failed and lose the path to recover… keep the failure and recovery controls persistently attached to the affected task, and use any toast as supplemental feedback only."* And: *"A toast may announce that failure happened, but it must not be the only place where the cause, consequence, support reference, or next action appears."*
- **Toasts should never auto-dismiss when actionable** — Twilio Paste: *"Toast error messages should never automatically disappear."* and *"Place the error message as close to the source of the error as possible."* ([paste.twilio.design/patterns/error-state](https://paste.twilio.design/patterns/error-state)).
- Smashing Magazine agrees toasts are a poor primary error surface (disconnected from cause, easy to miss) ([smashingmagazine.com/2022/08/error-messages-ux-design](https://www.smashingmagazine.com/2022/08/error-messages-ux-design/)).

### The "subtle for users, detailed for debugging" recipe

Combining severity-grading + progressive disclosure + optimistic-rollback UX:

1. **Transient, self-healing lags → whisper, don't shout.** A slow source that the ranker/retry logic auto-recovers from should surface as a *calm inline status change* ("This source is slow — trying another…"), not an alarming banner. Don't display an "error" for something the system is silently fixing (NN/g: don't display errors prematurely; a retry-in-progress isn't a failure yet).
2. **Actual failure → attach it to the now-playing/play surface, with a next action.** When stream-start truly fails, keep the failed item + a **"Couldn't start. Tap to retry."** control *on the play surface itself*, not only in a fleeting toast (toast-only anti-pattern). Persist until the user retries, picks another source, or dismisses.
3. **Retry copy names the action, offers the fix** — Linear-style: *"Could not send. Tap to retry."* not "Error 500" ([cadence.withremote.ai](https://cadence.withremote.ai/blog/optimistic-ui-react)).
4. **Detailed diagnostics behind progressive disclosure.** Peers/seeds/speed, the exact failing source, HTTP/ffmpeg detail → hidden by default, revealed via a "details" affordance or only when the wait crosses the budget. This satisfies both "don't alarm the normal user" (jargon hidden) and "capture detail for later diagnosis" (available on demand). On a **touchless surface** (if this remote ever renders on a TV/kiosk), remember per spela's own rule that hover-only detail is invisible — diagnostic detail must be a visible-on-demand element, not a `title=` tooltip.
5. **Capture for later diagnosis regardless of display.** Log the full transient/error detail (structured, timestamped) to the server/console even when the *user-facing* surface stays calm — the "subtly for users, detail for diagnosis" split. NN/g explicitly sanctions this: show codes *"for technical diagnostic purposes only."*

---

## 7. Consolidated cheat-sheet — exactly how each principle maps to the spela cast-remote

| # | Principle | Number | Concrete application |
|---|---|---|---|
| 1 | Instant press feedback | **<100 ms** (research floor ~30–85 ms visual) | Every transport button paints a `:active`/pressed state locally on tap, before any network call. `touch-action: manipulation` to kill the 300 ms mobile delay. |
| 2 | Keep flow of thought | **<1 s** | Acknowledgment of every command within ~400 ms (Doherty); result may land later behind an indicator. |
| 3 | Attention limit / need progress + cancel | **≥10 s** | Stream-start (torrent+transcode+cast) gets a percent-/step-textured warmup view + a way to cancel/pick another source. Never a bare eternal spinner. |
| 4 | Optimistic UI | perceived **<100 ms**, network hidden | Pause/resume/stop/seek/volume flip the UI immediately; reconcile from device status polling; roll back with 200–300 ms ease + retry toast on failure. Device state is authoritative (multi-controller/Superseded). |
| 5 | Minimal-by-default loading | show state only **>~300 ms**; spinner 2–10 s; %bar ≥10 s | Happy path: compact "Starting on [TV]… NN%". Anomaly (budget exceeded/stall): progressively disclose peers/speed/seed-health/steps. Plain language, no backend jargon. |
| 6 | Sender↔receiver sync | poll/push authoritative | Reconcile now-playing from the device even for changes the remote didn't make (other tabs, TV remote). `playing` is intent-only, never re-asserted from a status poll. |
| 7 | Persistent mini-controller | — | Device keeps playing when user navigates away; a persistent mini-controller (poster + play/pause + tap-to-expand) appears on every other view. Never stop on navigate-away. |
| 8 | Stop vs Disconnect | — | Default = Stop (halt on TV). Confirm success by *fading out* the now-playing surface + dropping the target indicator (the disappearance IS the confirmation). Optimistic + reconcile; restore + retry if the device fails to stop. |
| 9 | Severity-graded errors | — | Self-healing lags → calm inline status ("source slow, trying another"). Real failure → attached to the play surface with "Couldn't start. Tap to retry.", persistent (not toast-only). |
| 10 | Progressive-disclosure diagnostics | — | Seeds/peers/speed/HTTP/ffmpeg detail hidden by default, revealed on demand or on budget-exceed; always logged server-side for later diagnosis regardless of display. |

---

## Sources (primary, cited inline above)

- Nielsen, *Response Times: The 3 Important Limits* — https://www.nngroup.com/articles/response-times-3-important-limits/
- NN/g, *Website Response Times* — https://www.nngroup.com/articles/website-response-times/
- NN/g, *Powers of 10: Time Scales in UX* — https://www.nngroup.com/articles/powers-of-10-time-scales-in-ux/
- Yablonski, *Laws of UX — Ch.10 Doherty Threshold* — https://www.oreilly.com/library/view/laws-of-ux/9781098146955/ch10.html
- Doherty & Thadhani, *The Economic Value of Rapid Response Time* (archive) — https://archive.computerhistory.org/resources/access/text/2024/03/102751398-05-01-acc.pdf
- UXGen Studio, *The Doherty Threshold* — https://uxgenstudio.com/ux-laws/the-doherty-threshold/
- NN/g, *Progress Indicators Make a Slow System Less Insufferable* — https://www.nngroup.com/articles/progress-indicators/
- NN/g, *Skeleton Screens 101* — https://www.nngroup.com/articles/skeleton-screens/
- NN/g video, *Skeleton Screens vs. Progress Bars vs. Spinners* — https://www.nngroup.com/videos/skeleton-screens-vs-progress-bars-vs-spinners/
- NN/g, *Status Trackers and Progress Updates: 16 Design Guidelines* — https://www.nngroup.com/articles/status-tracker-progress-update/
- NN/g, *Designing for Long Waits and Interruptions* — https://www.nngroup.com/articles/designing-for-waits-and-interruptions/
- NN/g, *Progressive Disclosure* — https://www.nngroup.com/articles/progressive-disclosure/
- NN/g, *Error-Message Guidelines* — https://www.nngroup.com/articles/error-message-guidelines/
- Google Cast, *UX Guidelines* — https://developers.google.com/cast/docs/ux_guidelines
- Google Cast, *Sender App design checklist* — https://developers.google.com/cast/docs/design_checklist/sender
- Google Cast, *Cast Button checklist* — https://developers.google.com/cast/docs/design_checklist/cast-button
- Google Cast, *Cast Dialog checklist* — https://developers.google.com/cast/docs/design_checklist/cast-dialog
- Spotify, *Connect Basics* — https://developer.spotify.com/documentation/commercial-hardware/implementation/guides/connect-basics
- Chavan, *The "Triangle" Architecture: Spotify Connect* — https://medium.com/@rushichavan2327/the-triangle-architecture-a-deep-dive-into-spotify-connects-engineering-6c7d21f9f2a3
- Plex, *Cast from Browser or Desktop* — https://support.plex.tv/articles/201206866-cast-from-browser-or-desktop/
- Plex, *Controlling Flung Media* — https://support.plex.tv/articles/201165566-controlling-flung-media/
- React, *useOptimistic* — https://react.dev/reference/react/useOptimistic
- freeCodeCamp, *Optimistic UI with useOptimistic* — https://www.freecodecamp.org/news/how-to-use-the-optimistic-ui-pattern-with-the-useoptimistic-hook-in-react/
- Cadence, *Optimistic UI in React* — https://cadence.withremote.ai/blog/optimistic-ui-react
- Palma, *Optimistic UI with server reconciliation* — https://matheuspalma.com/blog/optimistic-ui-server-reconciliation-patterns
- UX Patterns Guide, *Toast-only critical error anti-pattern* — https://uxpatternsguide.com/patterns/toast-only-critical-error/
- Twilio Paste, *Error state* — https://paste.twilio.design/patterns/error-state
- Smashing Magazine, *Designing Better Error Messages UX* — https://www.smashingmagazine.com/2022/08/error-messages-ux-design/
- ACM TAP, *Towards the Temporally Perfect Virtual Button* (touch-feedback latency) — https://dl.acm.org/doi/10.1145/2611387
- Stack Overflow, *300 ms :active delay / touch-action: manipulation* — https://stackoverflow.com/questions/71676756/
- Atticus Li, *Psychology of Loading States* — https://atticusli.com/blog/posts/psychology-loading-states-perceived-performance/
- Vibe Coder, *Skeleton Screens & Loading Indicators Pattern Guide* — https://blog.vibecoder.me/skeleton-screens-loading-indicators-patterns
