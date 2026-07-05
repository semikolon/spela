# librqbit fast-start streaming research — engine-internal techniques to cut the "0-bytes-for-10-15s" cold-start on obscure swarms

Research date: 2026-07-05. Target: spela's Rust torrent engine (`src/torrent_engine.rs`, `src/torrent_stream.rs`) built on **librqbit** (`ikatson/rqbit`, `main` branch). Streaming model: ffmpeg reads a librqbit `FileStream` sequentially from byte 0 → transcode to HLS → cast.

Problem being attacked: obscure torrents connect peers but deliver **0 bytes for 10–15 s** (slow unchoke), and the sequential transcode can **stall waiting for the next in-order piece**.

**All source line references are against `crates/librqbit/src/…` on `ikatson/rqbit@main` as read on 2026-07-05.** Every `.rs` path below was fetched verbatim via the GitHub contents API.

---

## TL;DR — the cheapest high-impact wins

| # | Technique | Feasibility in librqbit | Impact on slow-start | Effort |
|---|---|---|---|---|
| **A** | **Inject ~15–25 public trackers via `SessionOptions.trackers`** (the session-level HashSet, NOT `AddTorrentOptions.trackers`) | **Already exposed — config knob.** The magnet path *silently ignores* `AddTorrentOptions.trackers`; the session-level set is merged into *every* torrent. | **High** for obscure magnets — 200–400% more peers found; faster initial bootstrap than DHT alone | ~10 lines |
| **B** | **Confirm/exploit FileStream sequential priority — it already exists** | **Already there.** `iter_next_pieces` feeds a 32 MB read-head window as `priority_pieces` into the picker. Nothing to build. | Already working; the stall you see is *starvation*, not lack of prioritization | 0 (verify only) |
| **C** | **The anti-stall "duplicate-request soonest piece from another peer" trick is already implemented** as `acquire_piece`'s **steal-from-slow-peer** (10×/3× thresholds) | **Already there.** But it's a *re-assignment*, not a true parallel duplicate request + cancel. See §2c for the nuance and the one upstream-patch idea worth it. | Medium-high; already prevents one slow peer from permanently owning the next needed piece | 0 (or upstream patch for true end-game) |
| **D** | **Enable a TCP listener + UPnP** (`SessionOptions.listen`) so you're *connectable* | **Config knob**, but **OFF by default** (`listen: None`). Being connectable roughly doubles reachable peers on a thin swarm. | Medium on obscure swarms | ~5 lines |
| **E** | Widen the read-head lookahead window (`PER_STREAM_BUF_DEFAULT = 32 MB`) | **Needs a tiny upstream patch** (it's a private `const`). Marginal — 32 MB is already generous. | Low | patch |
| **F** | Lower peer connect timeout / raise concurrency to get first bytes sooner | **Config knob** (`PeerConnectionOptions` + `concurrent_init_limit`) | Low-medium | ~3 lines |

**Do first:** A + D (both pure config, both target "0 bytes for 10-15s" directly by widening the peer set and making bootstrap faster), then verify B/C are behaving (they're already coded). E/upstream-end-game only if A+D+C don't close it.

---

## 1. librqbit's piece-priority / streaming APIs — what's ALREADY there (read the source)

**Verdict: librqbit already implements deadline-style streaming prioritization tied to the active `FileStream` read cursor.** You do not need to build sequential prioritization; it exists and is wired into the peer request loop. There is **no exposed public knob** to tune it (window size, aggressiveness) without an upstream patch, but the defaults are sensible.

### 1a. The read-head window → priority pieces (`torrent_state/streaming.rs`)

Every open `FileStream` registers a `StreamState` carrying its byte `position`. The engine computes a **32 MB look-ahead window of piece indices starting at the read cursor**:

```rust
// crates/librqbit/src/torrent_state/streaming.rs:29-30
// 32 mb lookahead by default.
const PER_STREAM_BUF_DEFAULT: u64 = 32 * 1024 * 1024;

// :45-52  — the per-stream queue is exactly the pieces from the read head forward, 32MB deep
fn queue<'a>(&self, lengths: &'a Lengths) -> impl Iterator<Item = ValidPieceIndex> + use<'a> {
    let start = self.file_abs_offset + self.position;
    let end = (start + PER_STREAM_BUF_DEFAULT).min(self.file_abs_offset + self.file_len);
    let dpl = lengths.default_piece_length();
    let start_id = (start / dpl as u64).try_into().unwrap();
    let end_id = end.div_ceil(dpl as u64).try_into().unwrap();
    (start_id..end_id).filter_map(|i| lengths.validate_piece_index(i))
}
```

These per-stream windows are interleaved across all active streams into the picker's priority list:

```rust
// streaming.rs:73-103
// Interleave 1st, 2nd etc pieces from each active stream in turn until they get 1/10th of the file.
pub(crate) fn iter_next_pieces<'a>(&'a self, lengths: &'a Lengths)
    -> impl Iterator<Item = ValidPieceIndex> + 'a { … Interleave { … } }
```

### 1b. The window is wired straight into the per-peer picker (`torrent_state/live/mod.rs`)

```rust
// crates/librqbit/src/torrent_state/live/mod.rs:1437-1447
let result = pieces.acquire_piece(AcquireRequest {
    peer: self.addr,
    peer_avg_time: self.counters.average_piece_download_time(),
    priority_pieces: self.state.streams.iter_next_pieces(&self.state.lengths), // <-- read-head window
    file_priorities,
    file_infos: &self.state.metadata.file_infos,
    peer_has_piece: |p| bf.get(p.get() as usize).map(|v| *v) == Some(true),
    can_steal: |p| self.state.per_piece_locks[p.get_usize()].try_write().is_some(),
});
```

So **every peer, every time it asks "what should I download next?", is handed the pieces nearest the transcoder's read cursor first.** This is the librqbit equivalent of libtorrent's `set_piece_deadline()` / sequential mode, done automatically the moment you call `torrent.stream(file_id)`.

### 1c. The picker prefers priority pieces, then steals from slow peers (`piece_tracker.rs`)

```rust
// crates/librqbit/src/piece_tracker.rs:517-567 (doc + impl)
/// The acquisition strategy is:
/// 1. Try to steal a piece from a peer that's 10x slower
/// 2. Try to reserve a piece from the queue (priority pieces first)
/// 3. Try to steal a piece from a peer that's 3x slower
pub fn acquire_piece<…>(&mut self, mut req: AcquireRequest<…>) -> AcquireResult {
    if let Some(result) = self.try_steal(&req, 10.0) { return result; }      // very slow peer
    for piece in &mut req.priority_pieces {                                   // <-- streaming window
        if !self.chunks.is_piece_have(piece) && !self.inflight.contains_key(&piece)
           && (req.peer_has_piece)(piece) { return self.reserve_piece(piece, req.peer); }
    }
    let queued: Vec<_> = self.chunks.iter_queued_pieces(req.file_priorities, req.file_infos).collect();
    for piece in queued { if (req.peer_has_piece)(piece) { return self.reserve_piece(piece, req.peer); } }
    if let Some(result) = self.try_steal(&req, 3.0) { return result; }        // moderately slow peer
    AcquireResult::NoneAvailable
}
```

`try_steal` finds an in-flight piece **owned by another peer that is ≥N× slower than the requesting peer**, and reassigns it (piece_tracker.rs:583-621). This is the built-in defence against "one slow peer stalls the soonest-needed piece."

### 1d. Files themselves are picked first-piece + last-piece first (moov atom) (`file_info.rs`)

```rust
// crates/librqbit/src/file_info.rs:13-25
// Iterate file pieces in the following order: first, last, everything else from start to end.
fn iter_piece_priorities(range) -> … { first.chain(last).chain(mid).take(r.len()) }
// test: it(0..4) == vec![0, 3, 1, 2]   (chunk_tracker.rs:236 iter_queued_pieces uses this)
```

So the **last piece (MP4 `moov` atom / MKV cues) is fetched early** already — you don't need to add first+last priority; it's default behaviour for the general queue (note: this is the *background* file-priority order, distinct from the stream read-head window in 1a, which dominates while a `FileStream` is open).

### 1e. The blocking read + wake mechanism (why the transcoder "stalls")

`FileStream::poll_read` checks `have_pieces[current.id]`; if the piece isn't there it **registers a waker and returns `Poll::Pending`** (streaming.rs:184-204). When a piece completes, `wake_streams_on_piece_completed` wakes exactly the streams whose current piece just arrived (streaming.rs:105-122). So the stall you observe is *correct back-pressure* — the transcoder is blocked because the **next in-order piece genuinely hasn't arrived yet**. The fix is therefore not "prioritize better" (already optimal) but "**get more/faster peers**" (§3) and "**don't let a slow peer own the critical piece**" (§2c).

### 1f. Exposed knobs summary (what you can actually set from spela without patching)

| Surface | Field | Streaming relevance |
|---|---|---|
| `torrent.stream(file_id).await` | — | **This is the trigger.** Opening a `FileStream` registers the read-head priority window. spela already does this. |
| `AddTorrentOptions.only_files: Option<Vec<usize>>` | select single file | Narrows the swarm's work to just your file — already used by spela (`only_files: Some(vec![idx])`). Good. |
| `AddTorrentOptions.initial_peers: Option<Vec<SocketAddr>>` | seed peers | If you ever cache peers per-infohash, feed them here to skip discovery latency. |
| `AddTorrentOptions.peer_limit`, `peer_opts` | connection tuning | §6 |
| **No public knob** | window size / steal thresholds / sequential aggressiveness | Hard-coded (`PER_STREAM_BUF_DEFAULT`, `10.0`/`3.0`). Change = upstream patch. |

**Minimal upstream patch if you ever want to tune the window/steal-aggressiveness:** expose `PER_STREAM_BUF_DEFAULT` and the `10.0`/`3.0` steal thresholds as fields on `SessionOptions` or `AddTorrentOptions` (they're currently module-private consts in `streaming.rs` and `piece_tracker.rs`). Low-risk, ~15 lines, plumbing only. **Not needed for the cold-start problem** — the defaults are fine; the problem is peer supply, not prioritization.

---

## 2. Streaming piece-selection algorithms (libtorrent / webtorrent / peerflix) — mapped onto librqbit

### 2a. `set_piece_deadline()` / deadline-based picking (libtorrent)
- **How it works:** libtorrent's `set_piece_deadline(piece, ms)` puts time-critical pieces into an "optimal" queue judged across *all* peers' request queues, and — critically — **requests a time-critical piece from the fastest available peer, and will duplicate-request it as the deadline nears**. Contrast with plain `set_sequential_range()` which just walks pieces in order per-peer. (arvidn/libtorrent Discussion #6272.) The maintainer notes the time-critical picker has known limits: it may under-fill the pipeline and under-probe peer rate.
- **librqbit equivalent:** `iter_next_pieces` (the read-head window) + `acquire_piece` priority-first is the sequential-range half. The **"fastest peer + duplicate near deadline"** half is only *partially* present (see 2c). librqbit has **no per-piece millisecond deadline API**; deadlines are implicit in the 32 MB window ordering.
- **Feasibility:** the sequential-range behaviour is **already there**; a true deadline API would be an **upstream patch** and is probably overkill for spela.

### 2b. Sequential sliding window at the read-head + rarest-first for the rest
- **How it works (webtorrent):** "seamlessly switches between sequential and rarest-first piece selection." Sequential/critical window near the playback head; once the buffer is deep enough, revert to **rarest-first** for the rest so you stay a good swarm citizen and web-peers don't get dropped for pure-leeching. (webtorrent README + FAQ; Issue #375.)
- **librqbit equivalent:** librqbit does the sequential-window half well. It does **NOT** do rarest-first for the tail — the background queue is `first,last,mid` file order (`file_info.rs`), i.e. **sequential-ish, not rarest-first**. For spela this is *fine and arguably better*: you transcode-and-discard, you don't care about swarm health or seeding, and a rarest-first tail would only slow the linear read. **No action.**
- **Note:** the academic "strict priority" rule (finish a partially-downloaded piece before starting a new one) is handled by librqbit's chunk tracker completing a reserved piece's chunks (`iter_chunk_infos` loop in the requester, live/mod.rs:1685+).

### 2c. **End-game / duplicate-request the soonest-needed piece from MULTIPLE peers** — the key anti-stall trick
- **How it works (BitTorrent end-game, Legout et al. "Rarest First and Choke Algorithms Are Enough"):** normally a block is requested from exactly one peer. In **end-game** (or, for streaming, near a critical deadline), the client requests the *same* not-yet-arrived block from **all** peers that have it, and **cancels the redundant requests** the moment one arrives. This is precisely the guard against a slow peer stalling an imminently-needed piece.
- **librqbit reality:** librqbit does **NOT** do true simultaneous duplicate requests + cancel. Instead it does **steal/re-assignment**: `try_steal` (piece_tracker.rs:583) detects that the peer currently downloading the needed piece is ≥10× (then ≥3×) slower than an available peer and **hands the piece to the faster peer** (single owner at a time; the slow peer's in-flight request is effectively abandoned). It also **cancels in-flight requests for a piece** when reassigning (`cancel_inflight_requests_for_piece`, peer/mod.rs:331) and has `late_cancelled_request_tolerance` for late arrivals. This is *most* of the benefit of end-game (a slow peer cannot permanently own the critical piece) but is **reactive** (waits until the piece has been in-flight long enough to look 3–10× slow) rather than **proactive** (fire at multiple peers immediately for the very first, most-critical pieces).
- **Why this matters for your exact symptom:** at t=0 on a fresh obscure swarm, the transcoder is blocked on piece 0. If the one peer that unchoked you is slow, librqbit's steal only kicks in after piece 0 has been in-flight ~3× the (still-unknown) average piece time — and at t=0 `average_piece_download_time()` may be `None`, disabling steal entirely (`try_steal` returns `None` when `peer_avg_time` is `None`, piece_tracker.rs:593). So **the first few pieces have the weakest anti-stall protection**, which is exactly the 10–15 s window you're seeing.
- **Cheapest mitigations (no upstream patch):**
  1. **§3 tracker injection + §4 connectability** — supply *more peers* so the picker has fast alternatives to steal toward, and so the first unchoke arrives sooner. This is the highest-leverage fix and does not require touching the picker.
  2. Accept a slightly larger pre-buffer before you start ffmpeg (spela's existing race-ahead gate) so the transcoder never catches a still-slow head. You already do this; it's the correct complement.
- **Upstream patch (optional, higher effort, real win for the first N pieces):** add **"proactive end-game for the first K pieces of an active stream"** — when a `FileStream` is at the very start and `average_piece_download_time()` is `None` or the head piece has been pending > ~2 s, request the head piece's chunks from *all* peers that have it, cancelling on first completion. This is the single most on-point upstream change for the "0 bytes for 10-15s" symptom, but A+D should be tried first (they're ~15 lines total vs a picker change).

### 2d. First + last piece priority (moov atom)
- **Already handled** by `iter_piece_priorities` = `first.chain(last).chain(mid)` (file_info.rs:13-25). No action. (For a fragmented MP4 the moov is at the front anyway; for a normal MP4 the last-piece-early default covers the tail-moov case.)

---

## 3. Swarm-widening for obscure torrents

### 3a. **Injecting extra public trackers — the #1 win, and there's a gotcha**

**Mechanism in librqbit (two paths, ONE of them silently drops your trackers on magnets):**

- **`AddTorrentOptions.trackers: Option<Vec<String>>`** — merged into the tracker set **only on the `.torrent`-file path**:
  ```rust
  // session.rs:1148-1150  (.torrent path)
  if let Some(custom_trackers) = opts.trackers.clone() { trackers.extend(custom_trackers); }
  ```
  On the **magnet path**, the tracker set is built *only* from `magnet.trackers` and **`opts.trackers` is never consulted**:
  ```rust
  // session.rs:1108-1116  (magnet path) — note: no reference to opts.trackers here
  InternalAddResult { info_hash, trackers: magnet.trackers.into_iter()
        .filter_map(|t| url::Url::parse(&t).ok()).collect(), … }
  ```
  **⚠️ spela uses magnet URIs, so `AddTorrentOptions.trackers` will be silently ignored.** Do not rely on it.

- **`SessionOptions.trackers: HashSet<url::Url>`** — merged into **every** torrent (magnet or file), applied late in `add_torrent_internal`:
  ```rust
  // session.rs:1550-1564
  if self.disable_trackers { trackers.clear(); }
  if is_private && trackers.len() > 1 {
      warn!(… "private trackers are not fully implemented, so using only the first tracker");
      trackers.truncate(1);
  } else if !self.disable_trackers {
      trackers.extend(self.trackers.iter().cloned());  // <-- SESSION-LEVEL injection, all torrents
  }
  ```
  **This is the reliable injection point.** It also **auto-respects the `private` flag** — on a private torrent it won't leak your file to public trackers (it truncates to the single original tracker). Perfect: obscure *public* torrents get the boost; private ones are left alone.

  rqbit's own CLI confirms this is the intended pattern: `--trackers` + `RQBIT_TRACKERS_FILENAME` both feed `SessionOptions.trackers` (main.rs:422-424, 565-580, 602-644: "Will append these to trackers from RQBIT_TRACKERS_FILENAME").

- **Canonical list (ngosang/trackerslist):**
  - Best-20 (recommended): `https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt` (curated 20 fastest, updated daily; mirrors: `https://ngosang.github.io/trackerslist/trackers_best.txt`, `https://cdn.jsdelivr.net/gh/ngosang/trackerslist@master/trackers_best.txt`).
  - `trackers_all.txt` exists but 50+ trackers = diminishing returns + many dead. **Use `trackers_best.txt` (or a hand-pinned subset).**

- **Expected impact:** on a thin swarm, peers are fragmented across discovery paths — "some peers only announce to specific trackers; going from 5 to 15 trackers can increase peer discovery by 200–400%." Sweet spot **~10–30 active trackers**; trackers give **faster initial bootstrap** than DHT (tracker announce returns a peer list in one round-trip; DHT needs iterative lookups). **Caveat that always applies:** trackers *find* peers, they don't *create* them — a genuinely 0-seed swarm stays 0-seed no matter how many trackers you add.

- **Concrete spela recommendation:** pin ~15–20 trackers from `trackers_best.txt` (bundle them at build time — don't fetch at runtime on the play path; refresh the bundled list periodically, e.g. weekly, à la spela's existing patterns) and pass them as `SessionOptions.trackers` when you build the `Session`. This is a **one-time session config**, not per-play, so it's ~10 lines in `torrent_engine.rs` `Session::new_with_opts`. This is the single highest-impact change and it targets the exact symptom (few peers → slow/late first unchoke).

### 3b. Ensuring DHT + PEX are active
- **DHT: ON by default.** `SessionOptions::default()` sets `dht: Some(DhtSessionConfig::default())` (session.rs:487). Keep it. (Only disabled if you pass `dht: None`.)
- **PEX: implemented and active.** librqbit sends `ut_pex` every 60 s and processes incoming PEX (live/mod.rs:910-982 `task_send_pex_to_peer`, `PEX_MESSAGE_INTERVAL = 60s`; peer_connection.rs handles `UtPex`). PEX only helps *after* you have ≥1 peer, so it compounds the tracker/DHT bootstrap rather than replacing it. No action needed — verify it's not disabled.
- **LSD (local service discovery):** also present (`disable_local_service_discovery: false` default). Irrelevant for obscure internet swarms but harmless.

### 3c. **Web seeds (BEP-19 `url-list` / BEP-17) — NOT supported in librqbit**
- **Grep result:** no `webseed` / `web_seed` / `url-list` / `url_list` / BEP-19/BEP-17 handling anywhere in `crates/librqbit/src`. librqbit is a pure peer/DHT/tracker client; it **ignores `url-list`** in the torrent metadata.
- **Impact:** web seeds would be the *ideal* cold-start fix (an always-available HTTP source for any piece, zero swarm dependency) — but this would be a **substantial upstream feature**, not a knob. **Not worth it for spela** given (a) most obscure release torrents don't carry `url-list` anyway, and (b) spela already has a better HTTP-fallback story at a different layer (its own Local-Bypass / serve-library curated copies). **Skip.**

### 3d. Peer/connection tuning + optimistic unchoke to get first bytes sooner
- **Connectability (being unchoked *by* peers vs. connecting *out*):** see §4/§D. On a thin swarm, being connectable (inbound) meaningfully increases the peer set.
- **Optimistic unchoke:** this is about *you* unchoking *others* (upload), which doesn't affect your *download* first-bytes latency. The thing that gets *you* unchoked faster is simply connecting to more peers (more chances that one unchokes you quickly) + being interesting (you're a fresh leecher, so peers' optimistic-unchoke slots are your main early door). **⇒ the lever is peer count (A+D), not an unchoke setting.**
- **`concurrent_init_limit` / connect timeout:** §6.

---

## 4 / §D. Enable a TCP listener + UPnP (connectability) — OFF by default

```rust
// session.rs:493  (SessionOptions::default)  →  listen: None   ⇒  NO inbound BitTorrent listener
```

With `listen: None`, spela only makes **outbound** connections. On a healthy swarm that's fine; on an **obscure** swarm, half the reachable peers may only be reachable if *they* can connect to *you*. Turning on a listener (+ UPnP port-forward) can roughly double your reachable peer set on thin swarms.

```rust
// pattern (mirrors rqbit main.rs:617)
use librqbit::{ListenerOptions, ListenerMode};
let listen = Some(ListenerOptions {
    mode: ListenerMode::TcpAndUtp,       // or TcpOnly
    listen_addr: "0.0.0.0:<port>".parse()?,   // pick a stable port
    enable_upnp_port_forwarding: true,   // helps behind NAT (Darwin is your router though — you can also just port-forward it)
    ..Default::default()
});
let sopts = SessionOptions { listen, trackers: my_trackers, ..Default::default() };
```

Since spela runs on Darwin (which is also the router), you can open the chosen port in nftables directly instead of / in addition to UPnP — cleaner and deterministic. **Feasibility: config knob, ~5 lines. Impact: medium on obscure swarms, near-zero cost.** Second-priority after tracker injection.

---

## 5 / §E. Widen the read-head lookahead window
`PER_STREAM_BUF_DEFAULT = 32 MB` (streaming.rs:30) is a **module-private const** → tuning it is an upstream patch. 32 MB ≈ 2–4 s of 1080p video buffered *ahead of the read head* for prioritization — already generous. **Not worth patching for cold-start** (the problem is supply, not window depth). Only revisit if you see mid-stream stalls *despite* healthy peer counts.

---

## 6 / §F. Peer connect-timeout / concurrency knobs (get first bytes sooner)

```rust
// PeerConnectionOptions (peer_connection.rs:71-80) — all Option, all exposed
pub struct PeerConnectionOptions {
    pub connect_timeout: Option<Duration>,      // lower ⇒ abandon dead peers faster, retry others
    pub read_write_timeout: Option<Duration>,
    pub keep_alive_interval: Option<Duration>,
}
// Settable via SessionOptions.connect / AddTorrentOptions.peer_opts.
// SessionOptions.concurrent_init_limit: Option<usize>  — how many torrents init concurrently
```

Request pipeline is already deep: `DEFAULT_PEER_REQUEST_WINDOW = 128` in-flight chunk requests per peer, further capped to the peer's advertised `reqq` (`request_window = reqq.min(128)`, live/mod.rs:995, 1167). So pipeline depth is **not** your bottleneck. On a thin swarm, a **shorter `connect_timeout`** (e.g. 10 s) lets spela cycle past dead/slow peers to reach a live one faster — a small, safe win. Note spela already sets 15/60/120 s connection timeouts; consider dropping the connect leg to ~10 s. **Feasibility: config knob. Impact: low-medium.**

---

## Bottom line for spela

1. **Ship tracker injection via `SessionOptions.trackers`** (bundle ~15–20 from `ngosang/trackerslist` `trackers_best.txt`; NOT `AddTorrentOptions.trackers` — that's dropped on the magnet path; the session path auto-respects `private`). **Highest impact, ~10 lines, targets the exact symptom.**
2. **Turn on an inbound TCP(+uTP) listener + open the port on Darwin** (`SessionOptions.listen`). ~5 lines, medium impact on obscure swarms, near-zero downside.
3. **Trust — and verify via logs — the built-ins**: `torrent.stream()` already installs a 32 MB read-head priority window (`iter_next_pieces` → `acquire_piece`), and the 10×/3× steal logic already re-assigns the critical piece away from a slow peer. DHT + PEX are on by default. Nothing to build here.
4. **Keep the existing race-ahead pre-buffer gate** — it's the correct complement to (3), covering the weakest-protection first-N-pieces window while `average_piece_download_time()` is still `None`.
5. **Only if 1–4 don't close the gap:** consider the one upstream patch that's genuinely on-point — **proactive end-game for the first K pieces of an active stream** (duplicate-request the head piece from all holders, cancel on first completion) — and/or expose `PER_STREAM_BUF_DEFAULT` / steal thresholds as config. **Skip web seeds** (unsupported, big feature, low real-world payoff for release torrents).

---

## Sources

**librqbit source (ikatson/rqbit@main, read 2026-07-05 via GitHub contents API):**
- `crates/librqbit/src/torrent_state/streaming.rs` — `PER_STREAM_BUF_DEFAULT`, `StreamState::queue`, `iter_next_pieces`, `FileStream::poll_read`, `wake_streams_on_piece_completed`, `Session::stream`
- `crates/librqbit/src/piece_tracker.rs` — `AcquireRequest.priority_pieces`, `acquire_piece` (steal 10×/3× → priority → queue → steal), `try_steal`
- `crates/librqbit/src/chunk_tracker.rs` — `iter_queued_pieces`, `reserve_needed_piece`
- `crates/librqbit/src/file_info.rs` — `iter_piece_priorities` (first, last, mid)
- `crates/librqbit/src/torrent_state/live/mod.rs` — `acquire_next_piece` call site (:1437), `DEFAULT_PEER_REQUEST_WINDOW=128` (:995), `task_peer_chunk_requester`, `task_send_pex_to_peer` (PEX, :910)
- `crates/librqbit/src/torrent_state/live/peer/mod.rs` — inflight requests, `cancel_inflight_requests_for_piece`, `late_cancelled_request_tolerance`
- `crates/librqbit/src/peer_connection.rs` — `PeerConnectionOptions`, `UtPex`/extended-message handling
- `crates/librqbit/src/session.rs` — `SessionOptions` (dht on, listen None, trackers empty), `AddTorrentOptions` (`trackers`, `only_files`, `initial_peers`), tracker-merge asymmetry (magnet :1108-1116 vs file :1148-1150 vs session-level :1550-1564)
- `crates/librqbit/src/listen.rs` — `ListenerOptions`, `ListenerMode`
- `crates/rqbit/src/main.rs` — CLI `--trackers` / `RQBIT_TRACKERS_FILENAME` → `SessionOptions.trackers`; listener setup pattern

**External references:**
- libtorrent `set_piece_deadline()` vs sequential — https://github.com/arvidn/libtorrent/discussions/6272
- webtorrent sequential/rarest-first switching — https://github.com/webtorrent/webtorrent (README), https://webtorrent.io/faq, https://github.com/webtorrent/webtorrent/issues/375
- peerflix / torrent-stream sequential streaming — https://github.com/mafintosh/peerflix , https://deepwiki.com/mafintosh/peerflix
- BitTorrent rarest-first / strict-priority / end-game policies — Legout et al., "Rarest First and Choke Algorithms Are Enough" — https://arxiv.org/pdf/cs/0609026
- ngosang public trackers list — https://github.com/ngosang/trackerslist , best-20: https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt
- Tracker-injection effectiveness on thin swarms / DHT-vs-tracker bootstrap — qBittorrent tracker guidance (geekchamp), BitComet wiki on PEX/DHT/trackers — https://wiki.bitcomet.com/peers-seeds-torrent-tracker-dht-peer-exchange-pex-magnet-links/
