# Playback readiness, identity, and end-of-media handoff

## What “ready” means

A percentage downloaded is not a contiguous playable prefix. Torrents can have many
pieces elsewhere while missing a header, the next byte VLC needs, or the tail index.
Head/tail access is necessary for container probing (MKV Cues and MP4 moov may be at
the tail), but is not proof that the opening video is available.

The live VLC readiness gate uses `TorrentEngine::startup_buffer` to verify an opening
prefix through librqbit FileStream with bounded reads. With both container ends present,
the normal opening target is 64 MiB (bounded by file size). The user-approved optimistic
path requires **six seconds** of observed new-byte delivery averaging at least **1.2×
the media bitrate**, current speed above that threshold, and a contiguous cushion of
at least **20 seconds and 16 MiB**. Unknown bitrate keeps the conservative prefix gate.
A completed torrent can bypass startup buffering. There is no 15%-of-file requirement.

This is a startup heuristic, not a guarantee of uninterrupted future delivery. Static
seed counts and cached bytes do not establish sustained health. Partial starts use the
opening position; a previous session's playhead must not contaminate readiness.

`BufferProgress` gives an active partial torrent 90 seconds without either opening-prefix
growth or new downloaded bytes before reporting a stall. New pieces elsewhere count as
progress: rarest-first delivery need not extend the prefix on every poll. Initialization
and polling gaps reset this observation window. This is distinct from the older
zero-byte/zero-peer race trigger (12 seconds, or 30 for large sources); do not describe
all fallback timers as one 90-second rule.

The HUD's `open_pct` measures progress toward the opening gate, not download completion.
Its wait reason, measured media information, and runway explain why an 11%-downloaded
file might still wait. On a new source, clear old measured quality/progress and reject
late responses belonging to the old request. Show source claims provisionally, then
replace them with information from the actual served file.

## Complete files and stable identity

Direct static serving requires full allocated-byte coverage (`allocation_covers_file`);
allowing even 5% missing can return zero-filled holes, corrupt frames, or premature EOF.
Allocation is a conservative check, not a checksum or proof that a densely allocated
file is uncorrupted. Incomplete owned files go through `adopt_owning_torrent` →
`resume_for` → piece-aware FileStream, which reuses present pieces and waits/downloads
the gaps. Do not declare files complete from logical size or playback duration.

Use each torrent's own output folder, including season-pack subdirectories. Playback
URLs identify the torrent **and file index**; result numbers alone belong to a mutable
search list. Bridge/watch tracking must use the title and IMDb ID resolved together
from that pinned identity. Otherwise an unrelated search can alter stream attribution,
resume state, or the title marked watched.

## Quality versus speed

The viewer prefers good 4K and reasonable startup speed, not the smallest file at any
cost. Missing codec metadata is uncertainty, not proof of inferior quality. Auto/1080p
share size priority within largest-anchored 10% bands in the same ranking group, allowing
seeds to choose among close alternatives. Large size differences and explicit 4K/Saver
preferences remain separate. See [the full ranking order](../README.md#how-sources-are-ranked).
Measured delivery governs readiness; ranking seed counts cannot replace it.

## Handoff and controls

Near the end, offer the next episode, with an easy Home action. (Recently watched
followed series were offered too until 2026-11-07; the server still returns them, the
remote no longer shows them.) Metadata can be prepared early; torrent downloads wait for
a deliberate play choice. Stop/next must await the VLC bridge's closure acknowledgement
before launching replacement playback; a failed stop preserves controls and shows a
persistent error. Natural completion should retain the end panel rather than leave a dead
player, and only completion: a VLC closed before the play counts as watched (the server's
rule, 96% or within five minutes of the end) returns to where the play started.

Single-key N/H overrides belong only in the end window, not throughout viewing. VLC
keeps its normal controls earlier; arrow keys remain untouched. The window is the final
4% capped at two minutes, for media at least five minutes long, not credit recognition.
Web and focused-VLC behavior differ after playback ends; exact scope, polling, stale
status handling, and installation are in the [shortcut integration](../integrations/hammerspoon/README.md).
The stronger Ctrl+Alt shortcuts remain available outside the end window.

## Superseded proposals worth retaining

The old gate combined head/tail availability with 15% and 64 MB. A measured 21.89 GB
4K file at 29 Mbps receiving 82 Mbit/s (2.8× realtime) still had to reach roughly
3.28 GB / 20.4 minutes of video, causing about five minutes of needless waiting.
Those numbers explain why a whole-file fraction was rejected, not a current threshold.

A proposed alternative was `runway >= D × (1 - r/b)`, floored around 20–30 seconds,
where D is duration, r download rate and b playback byte rate. This projects half-file
buffering at half-rate and a small floor above realtime, but assumes the observed rate
persists. It is **not the implemented gate**, nor is “wait for the full file whenever
rate is below bitrate” the current policy. Revisit sustained slow-swarm protection from
measurements, not by silently restoring the percentage rule.

Early proposals also called for adding global sequential piece priority and deduplicating
fire-and-forget prefetch tasks. librqbit already prioritizes first/last/middle pieces;
opening verification is now bounded and cancels on return. `prefetch_ends` and
`prefetch_at` still spawn detached reads; do not describe them as fully deduplicated or
cancelled. Audit those callers separately, and do not duplicate a priority mechanism
without checking its source.
DV profile-5 rendering quality remains a separate target-specific question, not a
justification for treating codec-less filenames as bad releases.

## Maintenance and verification

Inspect `server.rs` readiness/BufferProgress and `torrent_engine.rs` startup-buffer
helpers before changing thresholds. Tests should exercise sparse-file holes, actual
prefix progress versus aggregate progress, stale response rejection, and failed handoff.
Investigate stalls with identity, container readiness, verified prefix, delivery rate,
and initialization phase together. Do not infer a root cause from one percentage.

Never interrupt active viewing to validate a fix. The deployment script checks both
`/status` and `/progress` before restart. See [operations](../OPERATIONS.md) and
[availability checks](EPISODE_AVAILABILITY.md) for the distinct pre-play layers.

The completed-playback offer expires after five minutes and has a Dismiss button.
Expiry hides the overlay and disables its plain-key actions; it neither launches media
nor navigates away from the current page. Returning from a suspended/background tab
rechecks the wall-clock deadline. In-flight handoffs and actionable errors are retained.

The end-window predicate requires an active playback object. A retained near-end
playhead after VLC closes must not reopen an offer after Home, Dismiss or expiry.
Stop checks the title-scoped `gone` acknowledgement before queuing a command; an
already-closed player is a successful stop. Active-player stops still await cleanup.
Panel shortcut labels show N/H throughout the end offer, including while busy;
modifier shortcuts remain available outside that window.

On the Mac, the VLC bridge publishes play/pause state every second and wakes the
existing `monitor-brightness` launchd job when playback starts, pauses, resumes or ends. Pause returns to the
sun-driven baseline; resume restores viewing brightness; the five-minute daylight timer remains a fallback. Waking uses kickstart
without `-k`, preserving the single DDC writer, goodnight exclusion and hardware
verification. The viewing level remains 90, not 100. Implementation and machine
policy live in the user-local bridge/controller and dotfiles monitor-brightness doc.
