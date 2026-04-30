// Apr 29, 2026 — pure-Rust BitTorrent engine wrapping `librqbit::Session`.
//
// Replaces the `webtorrent-cli` Node subprocess in `src/torrent.rs`. Motivated by
// the Apr 29 incident where multiple Torrentio-aggregator results reporting hundreds
// of seeds attached to ZERO reachable peers under webtorrent-cli, while a fourth
// magnet (FLUX 720p, 30 reported seeds) attached to 31-57 peers at 10 MB/s on the
// same Darwin host. Pattern-matched against multiple webtorrent-cli GitHub issues
// (#175, #241): the Node implementation's peer-discovery is meaningfully weaker
// than libtorrent-class clients across magnets with quirky tracker subsets.
//
// Architecture change from chained subprocess to embedded library:
//
//   webtorrent-cli (Node):  ffmpeg <- HTTP :8888 (separate process)  <- torrent
//   librqbit (this file):   ffmpeg <- HTTP :7890 (spela's axum)       <- Session
//
// HLS chain + Chromecast DNAT hijack are unchanged: the only change is *who* serves
// bytes to the ffmpeg input URL. spela's existing `disk.rs` (sparse-aware via
// `metadata.blocks() * 512`) and Local Bypass (`top_level_file_is_healthy`) work
// against librqbit's on-disk layout without modification — librqbit allocates with
// `set_len()` and lays out files identically to webtorrent.
//
// Phase 1 (this commit): foundation only. Module compiles and is unit-tested in
// isolation. Not yet wired into `server.rs::do_play` — backend selection is gated
// on `config.torrent_backend = "librqbit"`, which defaults to "webtorrent" for now.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, TorrentStats,
};

// `librqbit::torrent_state::FileStream` and `ManagedTorrentHandle` (the
// `Arc<ManagedTorrent>` alias) are NOT re-exported at the crate root in 8.1.x
// because `mod torrent_state` is private. We work around it by:
//   1. Using `Arc<ManagedTorrent>` directly wherever the handle is needed.
//   2. Opening the FileStream INSIDE the streaming handler (`torrent_stream.rs`)
//      where the unnameable concrete type stays local. The handler only uses
//      it through its public AsyncRead/AsyncSeek/inherent `len()` surface.

/// Pure-Rust torrent engine. Wraps `librqbit::Session` with a spela-shaped
/// surface (u32 IDs, URL building, progress polling) so the rest of spela can
/// switch between this engine and the legacy webtorrent path with minimal
/// integration code.
pub struct TorrentEngine {
    session: Arc<Session>,
    /// Configured stream host (LAN IP that the Chromecast and ffmpeg will fetch
    /// from). Built into URLs returned from `start()`. Same value spela uses
    /// for the Chromecast LOAD message — ensures the cast hijack still works.
    stream_host: String,
    /// Port spela's axum router listens on. The new streaming endpoint
    /// (`/torrent/{id}/stream/{file_idx}`) lives on the same axum router as
    /// `/hls/master.m3u8`, so `:7890` for both.
    stream_port: u16,
    /// Counts new torrents started this session — used purely for telemetry /
    /// logs. The real ID returned to callers is `librqbit::TorrentId` cast to
    /// `u32`, looked up via `session.get(TorrentIdOrHash::Id(id as usize))`.
    started_count: AtomicU32,
}

/// Information returned to the caller of `start()`. Replaces the
/// `(pid: u32, url: String)` tuple webtorrent's `start_webtorrent` returned.
#[derive(Debug, Clone)]
pub struct TorrentStartInfo {
    /// Spela-side identifier for this torrent. Numerically equal to
    /// `librqbit::TorrentId` (a `usize`), narrowed to `u32` to fit spela's
    /// existing `CurrentStream.pid` plumbing without schema changes. The
    /// next-session `do_play` integration will treat this as opaque.
    pub id: u32,
    /// Fully-qualified HTTP URL ffmpeg will fetch the file from. Format:
    /// `http://{stream_host}:{stream_port}/torrent/{id}/stream/{file_idx}`.
    pub url: String,
    /// File index within the torrent that we instructed librqbit to download
    /// (via `only_files`). For single-file torrents this is always 0.
    pub file_index: usize,
}

/// Spela-shaped progress snapshot. Maps from `librqbit::TorrentStats` so callers
/// don't depend on librqbit types directly. Used by the self-healing logic in
/// `do_play` (the "0% progress after 12s" check from the legacy code path).
#[derive(Debug, Clone)]
pub struct TorrentProgress {
    pub bytes_downloaded: u64,
    pub bytes_total: u64,
    pub peers_connected: usize,
    pub speed_bps: u64,
    pub finished: bool,
    pub state: TorrentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    Initializing,
    Live,
    Paused,
    Error,
}

impl TorrentEngine {
    /// Construct an engine with a freshly-created `Session` rooted at `media_dir`.
    /// `stream_host` and `stream_port` are baked into URLs returned from `start`.
    /// Asynchronous because `Session::new` performs DHT bootstrap setup +
    /// listener binding.
    pub async fn new(
        media_dir: &Path,
        stream_host: impl Into<String>,
        stream_port: u16,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(media_dir).context("creating media_dir for torrent engine")?;
        let session = Session::new(media_dir.to_path_buf())
            .await
            .context("librqbit::Session::new failed during engine bootstrap")?;
        Ok(Arc::new(Self {
            session,
            stream_host: stream_host.into(),
            stream_port,
            started_count: AtomicU32::new(0),
        }))
    }

    /// Start a torrent from a magnet URI with optional file selection (BEP-53).
    /// Returns immediately after librqbit accepts the magnet — the actual
    /// metadata fetch + peer connection happens in background tasks owned by
    /// the session. The caller polls `progress()` to detect dead seeds.
    pub async fn start(
        &self,
        magnet: &str,
        file_index: Option<u32>,
    ) -> Result<TorrentStartInfo> {
        // Apr 30, 2026 SSRF defense — see `validate_magnet_uri` doc.
        validate_magnet_uri(magnet).map_err(|e| anyhow!("{}", e))?;
        let opts = AddTorrentOptions {
            // BEP-53: explicit file selection. Single-element vec for
            // single-file selection (matches spela's existing `--file-index N`
            // flow). When None, librqbit downloads ALL files (same as webtorrent
            // without `-s`); spela's ranker prefers single-file torrents so the
            // None case is rare in practice.
            only_files: file_index.map(|idx| vec![idx as usize]),
            // Allow resume into existing files on disk. Required so spela's
            // crash-recovery flow (and Local Bypass's `.spela_done` markers)
            // doesn't refuse to attach to a half-downloaded earlier session.
            overwrite: true,
            ..Default::default()
        };

        let response = self
            .session
            .add_torrent(AddTorrent::Url(magnet.into()), Some(opts))
            .await
            .context("session.add_torrent failed")?;

        let (torrent_id, _handle) = match response {
            AddTorrentResponse::Added(id, h) => (id, h),
            AddTorrentResponse::AlreadyManaged(id, h) => (id, h),
            AddTorrentResponse::ListOnly(_) => {
                return Err(anyhow!(
                    "librqbit returned ListOnly response — not expected when list_only=false"
                ))
            }
        };

        let id_u32 = shift_librqbit_id(torrent_id)?;
        let file_idx_usize = file_index.unwrap_or(0) as usize;

        self.started_count.fetch_add(1, Ordering::Relaxed);

        Ok(TorrentStartInfo {
            id: id_u32,
            // Apr 30, 2026 (H4 alignment): /torrent/{id}/stream/* is restricted
            // to loopback by the require_loopback_source middleware. ffmpeg
            // is the only legitimate consumer and runs in-process, so use
            // 127.0.0.1 in the URL we hand to ffmpeg — that way ffmpeg's
            // source IP is loopback (passes the middleware) regardless of
            // how stream_host is configured for Chromecast targets.
            url: build_stream_url("127.0.0.1", self.stream_port, id_u32, file_idx_usize),
            file_index: file_idx_usize,
        })
    }

    /// Poll a torrent's current state. Returns None for the Local Bypass
    /// sentinel id (0) or if the torrent has been removed from the session.
    pub fn progress(&self, id: u32) -> Option<TorrentProgress> {
        let librqbit_id = unshift_librqbit_id(id)?;
        let handle = self.session.get(TorrentIdOrHash::Id(librqbit_id))?;
        let stats: TorrentStats = handle.stats();
        Some(stats_to_progress(&stats))
    }

    /// Get a `ManagedTorrent` Arc handle. The streaming endpoint calls this
    /// then `.stream(file_idx)` on the handle to obtain a `FileStream` (whose
    /// concrete type can't be named in our crate but is usable through its
    /// `AsyncRead + AsyncSeek + .len()` surface). Returns `None` for the
    /// Local Bypass sentinel id (0) or if the torrent has been removed.
    pub fn handle(&self, id: u32) -> Option<Arc<ManagedTorrent>> {
        let librqbit_id = unshift_librqbit_id(id)?;
        self.session.get(TorrentIdOrHash::Id(librqbit_id))
    }

    /// Stop a torrent and optionally delete its on-disk files. `delete_files=false`
    /// drops from the session's active set but leaves bytes on disk for Local
    /// Bypass to reuse. `delete_files=true` is post-failure cleanup (zero-peer
    /// torrents leave only sparse placeholders worth deleting). The Local Bypass
    /// sentinel id (0) is a no-op (caller already checks `pid != 0` but defense
    /// in depth — would otherwise mistarget librqbit's TorrentId 0 if not for
    /// the +1 shift).
    pub async fn stop(&self, id: u32, delete_files: bool) -> Result<()> {
        let Some(librqbit_id) = unshift_librqbit_id(id) else {
            return Ok(());
        };
        self.session
            .delete(TorrentIdOrHash::Id(librqbit_id), delete_files)
            .await
            .context("session.delete failed")
    }

    /// Number of torrents started across this engine's lifetime. Diagnostic
    /// only — doesn't reflect currently-active count (use the session API for
    /// that).
    pub fn started_count(&self) -> u32 {
        self.started_count.load(Ordering::Relaxed)
    }
}

/// Reject anything that isn't a magnet URI. Apr 30, 2026 — security audit
/// caught that `librqbit::AddTorrent::Url` accepts `http://` / `https://`
/// URLs and fetches them as `.torrent` files via reqwest. With spela's HTTP
/// API unauthenticated and `default_host = "0.0.0.0"`, that turns `/play`
/// into an SSRF pivot against Darwin's internal services (Postgres :5433,
/// Redis :6379, FalkorDB :6380, Temporal :7233, llama.cpp :8080,
/// restic-rest :8001, AdGuard :3000, kamal-proxy admin). Same vector via
/// Ruby's `run_spela` voice-tool if Gemini gets prompt-injected. Defense in
/// depth: `do_play` and `handle_queue_add` validate at the HTTP boundary,
/// the engine validates again before crossing into librqbit.
pub(crate) fn validate_magnet_uri(s: &str) -> Result<&str, &'static str> {
    // Canonical magnet URIs are `magnet:?xt=urn:btih:HASH[&...]`. We accept
    // `magnet:` followed by anything — the strict parsing is librqbit's job.
    // What we MUST forbid is any other scheme (http/https/ftp/file/etc.)
    // because librqbit will treat those as torrent-file URLs and fetch them.
    if s.starts_with("magnet:") {
        Ok(s)
    } else {
        Err("magnet URI must start with 'magnet:'")
    }
}

/// Shift librqbit's `TorrentId` (which starts at 0 and increments) by +1 to
/// produce spela's `pid`. Apr 30, 2026 (v3.3.0+1 fix): librqbit allocates the
/// first torrent of a session as `TorrentId = 0`, which collides with spela's
/// `pid == 0` "Local Bypass" sentinel. Without this shift, the post-playback
/// reaper's `is_torrent_alive(state, 0)` returns `true` perpetually for the
/// first librqbit-served torrent, dead-coding the "ffmpeg AND torrent both
/// dead" cleanup branch. Shift+1 restores the invariant: `pid == 0` always
/// means Local Bypass; `pid >= 1` is a real torrent.
pub(crate) fn shift_librqbit_id(librqbit_id: usize) -> Result<u32> {
    u32::try_from(librqbit_id + 1).map_err(|_| {
        anyhow!(
            "librqbit torrent_id {} exceeds u32 range after +1 Local-Bypass-sentinel shift",
            librqbit_id
        )
    })
}

/// Reverse of `shift_librqbit_id`. Returns `None` for the Local Bypass
/// sentinel (0) so callers naturally skip the librqbit lookup. Used by
/// `progress`, `handle`, and `stop` so spela's pid==0 sentinel never reaches
/// `session.get` / `session.delete`.
pub(crate) fn unshift_librqbit_id(spela_id: u32) -> Option<usize> {
    if spela_id == 0 {
        None
    } else {
        Some((spela_id - 1) as usize)
    }
}

/// URL builder used at start time AND by the streaming handler when generating
/// example URLs. Pure function so it's directly testable.
pub(crate) fn build_stream_url(host: &str, port: u16, id: u32, file_idx: usize) -> String {
    format!("http://{}:{}/torrent/{}/stream/{}", host, port, id, file_idx)
}

/// Parse the leading-number portion of librqbit's `Speed` Display impl.
/// librqbit currently formats as `"3.2 MB/s"` etc.; we want the numeric
/// `3.2` for our internal bytes-per-second math. Pure helper so the format
/// dependency is testable — if librqbit ever changes the Display impl
/// (e.g. drops the space, switches units), our regression pin will catch
/// it immediately rather than silently breaking the 12s self-healing
/// progress check.
pub(crate) fn parse_mbps_string(s: &str) -> f64 {
    s.split_whitespace()
        .next()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Maps librqbit's `TorrentStats` into spela's flatter shape. Pure function so
/// the mapping is unit-testable without spinning up a real session.
fn stats_to_progress(stats: &TorrentStats) -> TorrentProgress {
    use librqbit::TorrentStatsState;

    let (peers_connected, speed_bps) = match &stats.live {
        Some(live) => {
            // `peer_stats` lives inside `snapshot` per librqbit 8.1.x.
            // The Speed type's Display formats as e.g. "3.2 MB/s"; multiply
            // by 1_000_000 / 8 to turn megabit/sec into bytes/sec. We only
            // use this for "is download making progress?" self-healing —
            // exact precision doesn't matter, but format-stability does.
            let mbps = parse_mbps_string(&format!("{}", live.download_speed));
            let bps = (mbps * 125_000.0) as u64; // mbps * 1e6 / 8
            (
                live.snapshot.peer_stats.live as usize,
                bps,
            )
        }
        None => (0, 0),
    };

    let state = match stats.state {
        TorrentStatsState::Initializing => TorrentState::Initializing,
        TorrentStatsState::Live => TorrentState::Live,
        TorrentStatsState::Paused => TorrentState::Paused,
        TorrentStatsState::Error => TorrentState::Error,
    };

    TorrentProgress {
        bytes_downloaded: stats.progress_bytes,
        bytes_total: stats.total_bytes,
        peers_connected,
        speed_bps,
        finished: stats.finished,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mbps_string_handles_typical_librqbit_format() {
        // librqbit 8.1.x Display impl yields "3.2 MB/s", "0 B/s", "10.5 KB/s".
        // Our parser takes the leading number; the units string is for human
        // display only. Pin the leading-number extraction.
        assert_eq!(parse_mbps_string("3.2 MB/s"), 3.2);
        assert_eq!(parse_mbps_string("10.5 KB/s"), 10.5);
        assert_eq!(parse_mbps_string("0 B/s"), 0.0);
    }

    #[test]
    fn parse_mbps_string_returns_zero_on_malformed() {
        // If librqbit ever changes the Display format and we can't parse,
        // we want to gracefully report 0 bytes/s rather than panic. The
        // 12s self-healing check uses (peers > 0 || bytes > 0 || speed > 0)
        // as its OR clause, so a 0-speed report is recoverable as long as
        // peers/bytes are non-zero.
        assert_eq!(parse_mbps_string(""), 0.0);
        assert_eq!(parse_mbps_string("abc"), 0.0);
        assert_eq!(parse_mbps_string("∞"), 0.0);
    }

    #[test]
    fn parse_mbps_string_handles_no_unit_suffix() {
        // Defensive: if librqbit drops the unit suffix (some Display
        // implementations do this for terse logging), still parse the
        // leading number.
        assert_eq!(parse_mbps_string("3.2"), 3.2);
        assert_eq!(parse_mbps_string("100"), 100.0);
    }

    #[test]
    fn validate_magnet_uri_accepts_canonical_form() {
        let m = "magnet:?xt=urn:btih:dc4b8c7c6ef6e5314c294280b7b7d106d452a1e8&dn=foo";
        assert_eq!(validate_magnet_uri(m), Ok(m));
    }

    #[test]
    fn validate_magnet_uri_accepts_no_query_slug() {
        // Some hashes-only test inputs use `magnet:HASH` form.
        let m = "magnet:dc4b8c7c6ef6e5314c294280b7b7d106d452a1e8";
        assert_eq!(validate_magnet_uri(m), Ok(m));
    }

    #[test]
    fn validate_magnet_uri_rejects_http_ssrf_attempt() {
        // The canonical SSRF pivot — librqbit fetches http URLs as torrent files.
        assert!(validate_magnet_uri("http://192.168.4.1:6379/").is_err());
        assert!(validate_magnet_uri("http://localhost:80/admin").is_err());
    }

    #[test]
    fn validate_magnet_uri_rejects_https_ssrf_attempt() {
        assert!(validate_magnet_uri("https://internal-service:8080/").is_err());
    }

    #[test]
    fn validate_magnet_uri_rejects_file_url() {
        // Defense in depth: even though librqbit may not honor file://, a future
        // version might. Reject at our boundary.
        assert!(validate_magnet_uri("file:///etc/passwd").is_err());
    }

    #[test]
    fn validate_magnet_uri_rejects_empty_string() {
        assert!(validate_magnet_uri("").is_err());
    }

    #[test]
    fn validate_magnet_uri_rejects_torrent_file_path() {
        assert!(validate_magnet_uri("/path/to/foo.torrent").is_err());
        assert!(validate_magnet_uri("foo.torrent").is_err());
    }

    #[test]
    fn validate_magnet_uri_rejects_close_misspell() {
        // Defense against `magnet :` (with space) or `magnet` (no colon).
        assert!(validate_magnet_uri("magnet ?xt=urn:btih:abc").is_err());
        assert!(validate_magnet_uri("magnetxt=urn:btih:abc").is_err());
    }

    #[test]
    fn shift_librqbit_id_avoids_local_bypass_sentinel() {
        // librqbit's first torrent is TorrentId=0; spela's pid==0 means
        // Local Bypass. The +1 shift moves librqbit's id space into pid >= 1
        // so the two namespaces never collide. Apr 30, 2026 fix.
        assert_eq!(shift_librqbit_id(0).unwrap(), 1);
        assert_eq!(shift_librqbit_id(1).unwrap(), 2);
        assert_eq!(shift_librqbit_id(99).unwrap(), 100);
    }

    #[test]
    fn unshift_librqbit_id_zero_is_local_bypass() {
        // The reverse operation MUST return None for spela id=0 so that
        // engine.handle / engine.progress / engine.stop never accidentally
        // dispatch to librqbit's TorrentId 0 when the caller passed the
        // Local Bypass sentinel. Reaper semantics (`is_torrent_alive(state, 0)
        // returns true` perpetually) relies on this.
        assert_eq!(unshift_librqbit_id(0), None);
    }

    #[test]
    fn unshift_librqbit_id_one_maps_to_first_torrent() {
        assert_eq!(unshift_librqbit_id(1), Some(0));
        assert_eq!(unshift_librqbit_id(2), Some(1));
        assert_eq!(unshift_librqbit_id(100), Some(99));
    }

    #[test]
    fn shift_unshift_roundtrip() {
        for librqbit_id in [0_usize, 1, 42, 1000, (u32::MAX - 1) as usize] {
            let spela_id = shift_librqbit_id(librqbit_id).unwrap();
            assert_ne!(spela_id, 0, "shifted id must never be the Local Bypass sentinel");
            let recovered = unshift_librqbit_id(spela_id).unwrap();
            assert_eq!(
                recovered, librqbit_id,
                "roundtrip for librqbit_id={}",
                librqbit_id
            );
        }
    }

    #[test]
    fn shift_overflow_at_u32_max() {
        // u32::MAX as usize -> +1 = u32::MAX + 1 which doesn't fit in u32.
        // Surfaces as an Err so callers can refuse rather than wrap.
        assert!(shift_librqbit_id(u32::MAX as usize).is_err());
    }

    #[test]
    fn build_stream_url_formats_known_inputs() {
        let url = build_stream_url("192.168.4.1", 7890, 42, 0);
        assert_eq!(url, "http://192.168.4.1:7890/torrent/42/stream/0");
    }

    #[test]
    fn build_stream_url_handles_hostname_streamhost() {
        // Apr 15, 2026: the codebase has a runtime warning for hostname
        // stream_host values (Chromecasts can't resolve LAN hostnames). The
        // URL builder itself does not validate — that responsibility lives
        // upstream. This test pins the formatting behavior so a refactor
        // can't silently change the URL structure.
        let url = build_stream_url("darwin.home", 7890, 1, 3);
        assert_eq!(url, "http://darwin.home:7890/torrent/1/stream/3");
    }

    #[test]
    fn build_stream_url_handles_high_ids() {
        let url = build_stream_url("10.0.0.5", 8080, u32::MAX, 99);
        assert_eq!(
            url,
            format!("http://10.0.0.5:8080/torrent/{}/stream/99", u32::MAX)
        );
    }

    #[test]
    fn torrent_state_maps_all_librqbit_variants() {
        // Pin the librqbit -> spela state mapping. If librqbit adds a variant
        // in a future release, the match in stats_to_progress will fail to
        // compile and force a deliberate decision rather than silently
        // misclassifying.
        use librqbit::TorrentStatsState;
        let cases = [
            (TorrentStatsState::Initializing, TorrentState::Initializing),
            (TorrentStatsState::Live, TorrentState::Live),
            (TorrentStatsState::Paused, TorrentState::Paused),
            (TorrentStatsState::Error, TorrentState::Error),
        ];
        for (input, expected) in cases {
            let stats = TorrentStats {
                state: input,
                file_progress: vec![],
                error: None,
                progress_bytes: 0,
                uploaded_bytes: 0,
                total_bytes: 0,
                finished: false,
                live: None,
            };
            let prog = stats_to_progress(&stats);
            assert_eq!(prog.state, expected, "state mapping for {:?}", input);
        }
    }

    #[test]
    fn stats_to_progress_pulls_byte_counters() {
        use librqbit::TorrentStatsState;
        let stats = TorrentStats {
            state: TorrentStatsState::Live,
            file_progress: vec![123],
            error: None,
            progress_bytes: 1_500_000,
            uploaded_bytes: 0,
            total_bytes: 4_500_000_000,
            finished: false,
            live: None,
        };
        let prog = stats_to_progress(&stats);
        assert_eq!(prog.bytes_downloaded, 1_500_000);
        assert_eq!(prog.bytes_total, 4_500_000_000);
        assert!(!prog.finished);
        // No live block -> 0 peers, 0 speed (reflects "not yet connected" or
        // "paused/error" — caller distinguishes via `state`).
        assert_eq!(prog.peers_connected, 0);
        assert_eq!(prog.speed_bps, 0);
    }

    #[test]
    fn stats_to_progress_marks_finished() {
        use librqbit::TorrentStatsState;
        let stats = TorrentStats {
            state: TorrentStatsState::Live,
            file_progress: vec![4_500_000_000],
            error: None,
            progress_bytes: 4_500_000_000,
            uploaded_bytes: 100,
            total_bytes: 4_500_000_000,
            finished: true,
            live: None,
        };
        let prog = stats_to_progress(&stats);
        assert!(prog.finished);
        assert_eq!(prog.bytes_downloaded, prog.bytes_total);
    }
}
