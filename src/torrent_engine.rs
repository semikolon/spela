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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long to wait for a magnet's metadata before calling the source dead. A healthy
/// swarm answers in well under a second; this ceiling exists for a slow-but-alive one.
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a proven-dead magnet is remembered. Short, because a swarm can recover and
/// a permanent blacklist would outlive the truth.
const DEAD_MAGNET_MEMORY: Duration = Duration::from_secs(10 * 60);
/// Recognisable by callers, which turn it into an actionable message rather than a spinner.
pub const NO_PEERS_ERROR: &str = "no peers — this source appears dead";

use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Magnet, ManagedTorrent,
    PeerConnectionOptions, Session, SessionOptions, SessionPersistenceConfig, TorrentStats,
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
    /// Port spela's axum router listens on. The new streaming endpoint
    /// (`/torrent/{id}/stream/{file_idx}`) lives on the same axum router as
    /// `/hls/master.m3u8`, so `:7890` for both.
    stream_port: u16,
    /// Counts new torrents started this session — used purely for telemetry /
    /// logs. The real ID returned to callers is `librqbit::TorrentId` cast to
    /// `u32`, looked up via `session.get(TorrentIdOrHash::Id(id as usize))`.
    started_count: AtomicU32,
    /// Magnets proven to have no reachable peers, with when that was proven. Bounds the
    /// cost of a dead source to ONE timeout rather than one per readiness poll.
    dead_magnets: Mutex<HashMap<String, Instant>>,
    /// Where librqbit writes files. Held because `ManagedTorrentShared::options` is
    /// private in 8.1.x, so a torrent cannot be asked where its own files live —
    /// `file_relative_path` returns a RELATIVE path for exactly this reason.
    media_dir: PathBuf,
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
    /// `http://127.0.0.1:{stream_port}/torrent/{id}/stream/{file_idx}`.
    ///
    /// CORRECTED 2026-08-28: this said `{stream_host}`. It has been LOOPBACK since the
    /// Apr-30 H4 hardening — `/torrent/*` is loopback-only and ffmpeg is its only
    /// legitimate consumer, so the URL must come from 127.0.0.1 to pass that middleware
    /// whatever `stream_host` is configured as. The engine no longer takes a stream host
    /// at all; keeping the parameter implied a control that did not exist.
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

/// Upload ceiling in BYTES per second — 100 Mbit/s, Fredrik's number (2026-09-09).
///
/// Darwin is the house ROUTER, so its uplink is shared with every device here and
/// saturating it costs everyone their latency, not just this download.
///
/// ⚠ librqbit's field is named `upload_bps` but the quota is spent in BYTES: the caller
/// is `prepare_for_upload(NonZeroU32::new(ci.size))` and `ci.size` is a chunk length in
/// bytes. Hence bits over eight.
const MAX_UPLOAD_BYTES_PER_SEC: u32 = 100_000_000 / 8;

impl TorrentEngine {
    /// Construct an engine with a freshly-created `Session` rooted at `media_dir`.
    /// `stream_port` is baked into the loopback URLs returned from `start`.
    /// Asynchronous because `Session::new` performs DHT bootstrap setup +
    /// listener binding.
    pub async fn new(media_dir: &Path, state_dir: &Path, stream_port: u16) -> Result<Arc<Self>> {
        std::fs::create_dir_all(media_dir).context("creating media_dir for torrent engine")?;
        // Apr 30, 2026 (L1 hardening — partial): librqbit 8.1.1 doesn't
        // expose a peer-count cap in SessionOptions / PeerConnectionOptions
        // (the librarian's research suggested one but it's not in this
        // version's API). What we CAN tune: per-peer connection timeouts
        // (slowloris defense) and concurrent_init_limit (caps how many
        // torrents can simultaneously bootstrap = bounded peak resource use).
        // Together these meaningfully reduce the long-running-embed FD
        // exhaustion class that rqbit issue #525 surfaced.
        // 2026-07-04: a SECOND (test) instance can't share the production
        // instance's DHT — the persistent DHT reloads its stored listen_addr
        // from the shared ~/.cache/com.rqbit.dht/dht.json and tries to bind
        // production's UDP port → "librqbit engine bootstrap failed".
        //   SPELA_EPHEMERAL_DHT=1 → DHT stays ON but skips persistence, so
        //     librqbit binds an ephemeral OS-assigned UDP port (0.0.0.0:0) and
        //     gets REAL peer discovery with zero collision. USE THIS for the
        //     test instance — trackers-only (SPELA_DISABLE_DHT) starves peer
        //     discovery and inflates every latency measurement.
        //   SPELA_DISABLE_DHT=1 → harder off-switch (trackers only), fallback.
        //   Neither set (production) → normal persistent DHT.
        let disable_dht = std::env::var("SPELA_DISABLE_DHT").is_ok();
        let ephemeral_dht = std::env::var("SPELA_EPHEMERAL_DHT").is_ok();
        // 2026-07-05: enable the inbound TCP peer listener (off by default in
        // librqbit → outbound-only). Being connectable ~doubles reachable peers
        // on thin/obscure swarms (Report B). Darwin IS the router, so no UPnP —
        // the port is opened manually in nftables (WAN iface) and forwarded to
        // this listener (spela runs on Darwin, so "forward" = a WAN INPUT accept).
        // Deterministic single port so the firewall rule matches: production
        // 6881, test instance 6882 (SPELA_TORRENT_PORT) to avoid a collision.
        let torrent_port: u16 = std::env::var("SPELA_TORRENT_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6881);
        // 2026-07-13: anti-piracy swarm-poisoning defense (Lever 1). librqbit
        // fetches a PeerGuardian P2P-format list (Name:start-end, gzip OK,
        // IPv4+IPv6 via ip_ranges.rs) and BANS matching peers at connect time.
        // Day-one decoy floods (anti-p2p firms + datacenter ranges) work by
        // filling connection slots with peers that handshake + advertise pieces
        // but never send bytes, so real seeders can't get in. Banning the known
        // ranges frees those slots. Default = the maintained Naunter aggregate
        // (~686k ranges: iblocklist level1 + bt). Override via SPELA_BLOCKLIST_URL;
        // set it empty to disable. Fetched once at session creation; a fetch
        // failure is non-fatal (librqbit logs + continues with no filter).
        let blocklist_url = match std::env::var("SPELA_BLOCKLIST_URL") {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(s),
            Err(_) => Some(
                "https://raw.githubusercontent.com/Naunter/BT_BlockLists/master/bt_blocklists.gz"
                    .to_string(),
            ),
        };
        let opts = SessionOptions {
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: Some(Duration::from_secs(15)),
                read_write_timeout: Some(Duration::from_secs(60)),
                keep_alive_interval: Some(Duration::from_secs(120)),
            }),
            concurrent_init_limit: Some(4),
            disable_dht,
            disable_dht_persistence: ephemeral_dht,
            // 2026-07-05: inject the expanded public-tracker list session-wide.
            // This is the RELIABLE injection path — librqbit merges
            // SessionOptions.trackers into EVERY torrent (session.rs:1564,
            // respecting the private flag) whereas AddTorrentOptions.trackers
            // is silently dropped on the magnet path (which spela uses). Widens
            // peer discovery on thin/obscure swarms = faster cold-start.
            // Research: docs/librqbit_streaming_faststart_research_2026_07_05.md.
            trackers: crate::search::PUBLIC_TRACKERS
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect(),
            ratelimits: librqbit::limits::LimitsConfig {
                upload_bps: std::num::NonZeroU32::new(MAX_UPLOAD_BYTES_PER_SEC),
                download_bps: None,
            },
            listen_port_range: Some(torrent_port..torrent_port + 1),
            enable_upnp_port_forwarding: false,
            blocklist_url,
            // 2026-09-09: REMEMBER torrents across a restart, and remember which
            // pieces are already good.
            //
            // Without this every play began from nothing: a magnet fetch against the
            // swarm, then `Doing initial checksum validation` re-reading whatever was
            // already on disk (measured 22s for 1.8 GiB of a season pack on the HDD).
            // We threw the knowledge away at teardown and then paid to rebuild it.
            //
            // The two settings are bundled ONE way only, and the direction matters:
            // `persistence_factory` hands back `NonPersistentBitVFactory` whenever
            // `persistence` is None, so fastresume REQUIRES persistence (the piece
            // record lives in the same store). The half that sounds alarming —
            // "everything resumes downloading on restart" — is not bundled at all: the
            // saved record carries `is_paused` and `into_add_torrent` restores it, so a
            // torrent paused at teardown comes back PAUSED. Known, indexed, silent, and
            // no argument with the 100 GB pruner.
            //
            // The store lives in the STATE dir, never the media dir: the pruner owns the
            // media dir and would eventually delete its own bookkeeping.
            // Remember torrents and which pieces are already good (2026-09-09).
            //
            // Without this every play began from nothing: a magnet fetch against the
            // swarm, then a full re-read of whatever was already on disk. We threw the
            // knowledge away at teardown and then paid to rebuild it.
            //
            // Bundled ONE way only: `persistence_factory` returns
            // `NonPersistentBitVFactory` whenever `persistence` is None, so fastresume
            // REQUIRES persistence. The half that sounds alarming, "everything resumes
            // downloading on restart", is not bundled at all — the saved record carries
            // `is_paused` and `into_add_torrent` restores it, so a torrent paused at
            // teardown comes back paused, and `settle_restored_torrents` pauses the rest.
            //
            // ⚠ This was ON, then OFF, then on again within two hours. It was turned off
            // on the theory that it caused unbounded peer-socket growth; the growth
            // continued unchanged after the revert, so the theory was wrong. The real
            // cause was librqbit's incoming listener starving its own drain (see the
            // fork pinned in Cargo.toml), and the router now caps inbound connections at
            // 300 regardless. Both of those are upstream of this setting.
            //
            // The store lives in the STATE dir, never the media dir: the pruner owns the
            // media dir and would eventually delete its own bookkeeping.
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(state_dir.join("librqbit")),
            }),
            fastresume: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(media_dir.to_path_buf(), opts)
            .await
            .context("librqbit::Session::new_with_opts failed during engine bootstrap")?;
        let engine = Arc::new(Self {
            session,
            stream_port,
            started_count: AtomicU32::new(0),
            dead_magnets: Mutex::new(HashMap::new()),
            media_dir: media_dir.to_path_buf(),
        });
        engine.settle_restored_torrents().await;
        Ok(engine)
    }

    /// Physical bytes a set of files actually holds. Sparse placeholders read as 0.
    ///
    /// `len()` is a LIE for a torrent file: librqbit creates every selected file at its
    /// full logical size immediately, so a placeholder holding nothing reports gigabytes.
    /// Only the block count says what is really there.
    fn physical_bytes(paths: impl Iterator<Item = PathBuf>) -> u64 {
        use std::os::unix::fs::MetadataExt;
        paths
            .filter_map(|p| std::fs::metadata(&p).ok())
            .map(|m| m.blocks() * 512)
            .sum()
    }

    /// Put the session in a safe state after a restart.
    ///
    /// Two things are owed here, and both exist because persistence is now on.
    ///
    /// PAUSE EVERYTHING. Nothing is playing at boot. A torrent restored from the store
    /// comes back in whatever state it was persisted in, and the normal end of a live
    /// stream is `TimeoutStopSec` followed by SIGKILL — no chance to pause — so a
    /// torrent that was downloading when spela was killed comes back downloading. It
    /// would then quietly pull gigabytes nobody asked for, competing for the same disk
    /// the pruner is trying to free. A play un-pauses exactly what it needs.
    ///
    /// FORGET THE EMPTY ONES. The pruner deletes files and knows nothing about
    /// librqbit, so its evictions leave records behind; librqbit recreates those files
    /// as placeholders when it restores the torrent. That is why the test is PHYSICAL
    /// BYTES rather than "does the file exist" — by the time this runs the file always
    /// exists, at full logical size, holding nothing. Exactly zero blocks across every
    /// file is the only condition that deletes, so there is nothing to lose when it does.
    async fn settle_restored_torrents(&self) {
        let root = self.media_dir.clone();
        // PAUSE FIRST, and pause everything — including torrents whose metadata has not
        // resolved yet. Pausing needs no knowledge of the files; measuring does. Gating
        // the pause on metadata therefore left a race in which a torrent restored
        // mid-resolution kept downloading, which is exactly what happened on the first
        // live restart after this shipped: the pause line never appeared. Order matters
        // more than tidiness here, because a pause is free and reversible and the whole
        // point is that nothing fetches anything until a play asks for it.
        let all: Vec<usize> = self
            .session
            .with_torrents(|it| it.map(|(id, _)| id).collect());
        for id in all {
            let Some(handle) = self.session.get(TorrentIdOrHash::Id(id)) else {
                continue;
            };
            if handle.is_paused() {
                continue;
            }
            match self.session.pause(&handle).await {
                Ok(()) => tracing::info!(
                    "librqbit: paused restored torrent {} — nothing is playing yet",
                    id
                ),
                // An initializing torrent can refuse, and the next boot catches it.
                // Never fatal: an engine that will not start is far worse than a torrent
                // that downloads when it should not.
                Err(e) => {
                    tracing::debug!("librqbit: could not pause restored torrent {}: {}", id, e)
                }
            }
        }
        let empty: Vec<(usize, u64)> = self.session.with_torrents(|it| {
            let mut empty = Vec::new();
            for (id, t) in it {
                let Some(meta) = t.metadata.load_full() else {
                    // Metadata unresolved: no evidence about what is on disk, so leave
                    // it. It is paused above, which is the half that matters.
                    continue;
                };
                let bytes = Self::physical_bytes(
                    meta.file_infos
                        .iter()
                        .map(|fi| root.join(&fi.relative_filename)),
                );
                if bytes == 0 {
                    empty.push((id, bytes));
                }
            }
            empty
        });
        for (id, bytes) in empty {
            // NEVER DELETES FILES (2026-09-09, tightened the same evening it shipped).
            // It forgets the RECORD only. The measurement said zero blocks, so the
            // placeholders it leaves behind cost nothing, and the pruner owns the media
            // directory anyway — a boot-time reconciler has no business removing media.
            //
            // The tightening was not prompted by a proven fault in this function: a
            // partial download went missing across a restart the same evening and this
            // was ONE of the few things that could delete a file, so the capability was
            // removed rather than defended. Cheap, and it takes this code out of the
            // suspect list for good.
            //
            // The measurement still goes in the log beside the verdict, so it can be
            // compared against librqbit's own `Initial check results: have N` a few
            // lines above in the same journal.
            match self.session.delete(TorrentIdOrHash::Id(id), false).await {
                Ok(()) => tracing::info!(
                    "librqbit: forgot torrent {} — {} physical bytes on disk (files left alone)",
                    id,
                    bytes
                ),
                Err(e) => tracing::warn!("librqbit: could not forget torrent {}: {}", id, e),
            }
        }
    }

    /// Finished torrents that are still uploading, with the name of their content.
    ///
    /// The caller decides which to stop, because that decision needs the watch ledger
    /// and the engine has no business knowing about it. Fredrik's rule (2026-09-09): a
    /// finished download KEEPS SEEDING until he has watched it, or until the pruner
    /// takes the files. Sharing back is the decent default; what was never decided was
    /// seeding things nobody is going to watch, forever.
    ///
    /// Why a caller-driven sweep rather than a hook at completion: the finished state is
    /// reached asynchronously inside librqbit, so a check at completion, or at boot,
    /// races that and loses. A periodic sweep cannot lose a race it does not enter.
    pub fn finished_and_seeding(&self) -> Vec<(u32, String)> {
        self.session.with_torrents(|it| {
            it.filter_map(|(id, t)| {
                if t.is_paused() || !t.stats().finished {
                    return None;
                }
                let name = t.name().unwrap_or_default();
                shift_librqbit_id(id).ok().map(|sid| (sid, name))
            })
            .collect()
        })
    }

    /// Stop a torrent uploading, keeping it and its files exactly where they are.
    pub async fn pause_seeding(&self, id: u32) -> Result<()> {
        let Some(librqbit_id) = unshift_librqbit_id(id) else {
            return Ok(());
        };
        let Some(handle) = self.session.get(TorrentIdOrHash::Id(librqbit_id)) else {
            return Ok(());
        };
        if handle.is_paused() {
            return Ok(());
        }
        self.session
            .pause(&handle)
            .await
            .context("session.pause failed")
    }

    /// Which torrent owns this file on disk, and which file it is inside it.
    ///
    /// The season-pack case, 2026-09-06: an episode showed "57% downloaded" and would
    /// not play. Both halves of that were honest. The bytes really were on disk and
    /// really did belong to that release — but they belonged to a DIFFERENT torrent,
    /// the season pack, while the row clicked was the standalone whose swarm has no
    /// peers at all. Local Bypass found the file and refused it for being full of
    /// holes, which was correct, and then nobody asked the next question: is something
    /// already fetching this? It was.
    ///
    /// Answering it turns that click into an instant play with the holes filling as it
    /// goes, which is what streaming from a partial torrent does anyway.
    pub fn owner_of(&self, path: &Path) -> Option<(u32, usize)> {
        let target = std::fs::canonicalize(path).ok()?;
        let root = self.media_dir.clone();
        self.session.with_torrents(|it| {
            for (id, t) in it {
                let Some(meta) = t.metadata.load_full() else {
                    continue;
                };
                for (idx, fi) in meta.file_infos.iter().enumerate() {
                    let candidate = root.join(&fi.relative_filename);
                    if std::fs::canonicalize(&candidate).ok().as_deref() == Some(target.as_path()) {
                        return shift_librqbit_id(id).ok().map(|sid| (sid, idx));
                    }
                }
            }
            None
        })
    }

    /// Un-pause a torrent we are about to stream from, and make sure the file we want
    /// is one it is actually fetching. A paused torrent serves whatever is already on
    /// disk and never fills the gaps, which reads as a stall rather than as a pause.
    pub async fn resume_for(&self, id: u32, file_idx: usize) -> Result<()> {
        let Some(librqbit_id) = unshift_librqbit_id(id) else {
            return Ok(());
        };
        let Some(handle) = self.session.get(TorrentIdOrHash::Id(librqbit_id)) else {
            return Ok(());
        };
        if handle
            .only_files()
            .as_ref()
            .is_some_and(|f| !f.contains(&file_idx))
        {
            let mut want: std::collections::HashSet<usize> = handle
                .only_files()
                .unwrap_or_default()
                .into_iter()
                .collect();
            want.insert(file_idx);
            if let Err(e) = self.session.update_only_files(&handle, &want).await {
                tracing::warn!("librqbit: could not widen file selection: {}", e);
            }
        }
        if handle.is_paused() {
            self.session
                .unpause(&handle)
                .await
                .context("session.unpause failed")?;
            tracing::info!(
                "librqbit: resumed torrent {} to fill in file {}",
                id,
                file_idx
            );
        }
        Ok(())
    }

    /// The torrent we already hold for this magnet, if any — WITHOUT touching the swarm.
    ///
    /// This is the line the whole persistence change exists for. librqbit's own
    /// "already managed" check lives in `add_torrent_internal` and runs AFTER
    /// `resolve_magnet`, so handing `add_torrent` a magnet we already hold still pays
    /// the full metadata fetch: instant on a live swarm, `METADATA_TIMEOUT` on a dead
    /// one. The infohash is right there in the magnet, and the session is a map, so the
    /// answer costs a lookup.
    fn already_managed(&self, magnet: &str) -> Option<Arc<ManagedTorrent>> {
        let m = Magnet::parse(magnet).ok()?;
        let hash = m.as_id20()?;
        self.session.get(TorrentIdOrHash::Hash(hash))
    }

    /// Start a torrent from a magnet URI with optional file selection (BEP-53).
    /// Returns immediately after librqbit accepts the magnet — the actual
    /// metadata fetch + peer connection happens in background tasks owned by
    /// the session. The caller polls `progress()` to detect dead seeds.
    pub async fn start(&self, magnet: &str, file_index: Option<u32>) -> Result<TorrentStartInfo> {
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

        // Already ours? Then say so now, and never speak to the swarm.
        //
        // This is the line the persistence change exists for. librqbit HAS an
        // already-managed check, but it sits in `add_torrent_internal` AFTER
        // `resolve_magnet`, so handing it a magnet we already hold still pays the full
        // metadata fetch — instant on a live swarm, `METADATA_TIMEOUT` on a dead one.
        // Since teardown now pauses rather than deletes, a source played before is
        // still here with its piece record intact, and this turns a 20-second wait into
        // a map lookup. A dead swarm stops mattering for anything already downloaded.
        if let Some(handle) = self.already_managed(magnet) {
            // The file wanted may not be the file selected. A season pack added for
            // episode 6 must widen its selection before it will fetch episode 7,
            // otherwise the stream waits on pieces nobody asked for.
            if let Some(idx) = file_index {
                let idx = idx as usize;
                let selected = handle.only_files();
                let needs_widening = selected.as_ref().is_some_and(|f| !f.contains(&idx));
                if needs_widening {
                    let mut want: std::collections::HashSet<usize> =
                        selected.unwrap_or_default().into_iter().collect();
                    want.insert(idx);
                    if let Err(e) = self.session.update_only_files(&handle, &want).await {
                        tracing::warn!("librqbit: could not widen file selection: {}", e);
                    }
                }
            }
            if handle.is_paused() {
                self.session
                    .unpause(&handle)
                    .await
                    .context("unpausing an already-managed torrent")?;
                tracing::info!(
                    "librqbit: resumed torrent {} (already on disk)",
                    handle.id()
                );
            }
            let id_u32 = shift_librqbit_id(handle.id())?;
            return Ok(TorrentStartInfo {
                id: id_u32,
                file_index: file_index.unwrap_or(0) as usize,
                url: format!(
                    "http://127.0.0.1:{}/torrent/{}/stream/{}",
                    self.stream_port,
                    id_u32,
                    file_index.unwrap_or(0)
                ),
            });
        }

        // A magnet carries no metadata — librqbit must fetch it from the swarm before it
        // can report a single byte. With NO reachable peers that await never returns, and
        // it had no bound of any kind: every caller inherited an indefinite hang. That is
        // what a dead source looked like from the outside — not an error, not slowness,
        // just a request that never answered, so /vlc/ready stopped responding and the
        // web remote sat on "Connecting to peers…" forever with nothing to report
        // (The Diplomat S03E01, MeGusta release, 2026-08-27).
        //
        // A healthy magnet resolves in well under a second. The generous ceiling here is
        // to protect a slow-but-alive swarm, not to wait out a dead one.
        if let Some(at) = self.dead_magnets.lock().unwrap().get(magnet) {
            if at.elapsed() < DEAD_MAGNET_MEMORY {
                // Already proven unreachable moments ago. Answer at once rather than
                // making every ~1s readiness poll re-serve the full timeout — otherwise
                // the UI is still stuck, just in slower increments.
                return Err(anyhow!("{}", NO_PEERS_ERROR));
            }
            self.dead_magnets.lock().unwrap().remove(magnet);
        }
        let added = tokio::time::timeout(
            METADATA_TIMEOUT,
            self.session
                .add_torrent(AddTorrent::Url(magnet.into()), Some(opts)),
        )
        .await;
        let response = match added {
            Ok(r) => r.context("session.add_torrent failed")?,
            Err(_) => {
                self.dead_magnets
                    .lock()
                    .unwrap()
                    .insert(magnet.to_string(), Instant::now());
                tracing::warn!(
                    "librqbit: no peers for magnet after {:?} — treating source as dead",
                    METADATA_TIMEOUT
                );
                return Err(anyhow!("{}", NO_PEERS_ERROR));
            }
        };

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
    /// End a torrent's activity.
    ///
    /// `delete_files: true` throws everything away — the files and the record — and is
    /// for a failed start, whose sparse placeholder is worth nothing.
    ///
    /// `delete_files: false` means "stop downloading but KEEP the bytes", and since
    /// 2026-09-09 that is a PAUSE rather than a delete. Deleting also erased librqbit's
    /// record of the torrent, including which pieces were already good, so the next play
    /// of the same release started from nothing: a fresh magnet fetch against the swarm
    /// and a full re-read of the file to work out what we already had. We discarded the
    /// knowledge and then paid to rebuild it. A paused torrent keeps its record, resumes
    /// instantly, and downloads nothing meanwhile, so the pruner still owns the disk.
    pub async fn stop(&self, id: u32, delete_files: bool) -> Result<()> {
        let Some(librqbit_id) = unshift_librqbit_id(id) else {
            return Ok(());
        };
        if delete_files {
            return self
                .session
                .delete(TorrentIdOrHash::Id(librqbit_id), true)
                .await
                .context("session.delete failed");
        }
        match self.session.get(TorrentIdOrHash::Id(librqbit_id)) {
            Some(handle) => {
                if handle.is_paused() {
                    return Ok(());
                }
                self.session
                    .pause(&handle)
                    .await
                    .context("session.pause failed")
            }
            // Already gone. Nothing to keep and nothing to stop.
            None => Ok(()),
        }
    }

    /// Number of torrents started across this engine's lifetime. Diagnostic
    /// only — doesn't reflect currently-active count (use the session API for
    /// that).
    /// Best-effort "download first AND last pieces first" primer (2026-08-03).
    /// A player probes the container before playback — track headers (front) + the
    /// seek index (MKV **Cues** / MP4 **moov**), and MKV keeps its Cues at the END.
    /// Torrents fetch rarest-first, so the tail (Cues) often isn't present and VLC's
    /// end-probe BLOCKS on it → "waits forever" on a fresh/partial torrent. Reading a
    /// byte range through a `FileStream` makes librqbit PRIORITIZE those pieces, so
    /// this opens a stream and reads the last + first couple MB, priming exactly the
    /// pieces VLC needs to start. Fire-and-forget: spawned, all errors swallowed,
    /// NEVER blocks the caller or the serve path (safe to stage unverified — it's
    /// additive and isolated). The read of a not-yet-downloaded tail simply waits
    /// while librqbit fetches it with priority; that's the point.
    /// On-disk path of one file in a torrent, for measuring what is actually there.
    ///
    /// Needed because the runway measurement reads the FILE (one `lseek(SEEK_HOLE)`)
    /// rather than a piece bitmap — librqbit's `TorrentStats` exposes no bitmap, and the
    /// file is the ground truth anyway.
    /// RELATIVE path — librqbit's output folder lives on a crate-private options struct,
    /// so the caller joins this with the media dir it already knows.
    pub fn file_relative_path(&self, id: u32, file_idx: usize) -> Option<PathBuf> {
        let handle = self.handle(id)?;
        handle
            .with_metadata(|m| {
                m.file_infos
                    .get(file_idx)
                    .map(|fi| fi.relative_filename.clone())
            })
            .ok()?
    }

    pub fn prefetch_ends(&self, id: u32, file_idx: usize) {
        let Some(handle) = self.handle(id) else {
            return;
        };
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            const CHUNK: u64 = 2 * 1024 * 1024; // 2 MB per end
                                                // Tail first — the MKV Cues live at the end; that's the piece VLC blocks on.
            if let Ok(mut s) = handle.clone().stream(file_idx) {
                let len = s.len();
                if len > CHUNK && s.seek(std::io::SeekFrom::Start(len - CHUNK)).await.is_ok() {
                    let mut buf = vec![0u8; CHUNK as usize];
                    let _ = s.read_exact(&mut buf).await;
                }
            }
            // Head — container header / track info (also served first for playback).
            if let Ok(mut s) = handle.stream(file_idx) {
                let mut buf = vec![0u8; CHUNK as usize];
                let _ = s.read_exact(&mut buf).await;
            }
        });
    }

    /// Prioritize a window starting at `byte_offset` (a mid-file seek target) so the
    /// pieces VLC is about to read download FIRST — keeps a resumed play from
    /// starving at the seek point on a fresh torrent, where the default piece order
    /// hasn't reached that region. Fire-and-forget; overlaps with VLC's own read
    /// harmlessly (librqbit dedupes pieces). Aug 3 2026: Fargo S01E05 resume-to-41min
    /// froze because byte ~3.1 GB wasn't downloaded and nothing was prioritizing it.
    pub fn prefetch_at(&self, id: u32, file_idx: usize, byte_offset: u64) {
        let Some(handle) = self.handle(id) else {
            return;
        };
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            const WINDOW: usize = 8 * 1024 * 1024; // ~13s at 5 Mbps of runway
            if let Ok(mut s) = handle.stream(file_idx) {
                let len = s.len();
                if byte_offset < len && s.seek(std::io::SeekFrom::Start(byte_offset)).await.is_ok()
                {
                    let want = WINDOW.min((len - byte_offset) as usize);
                    let mut buf = vec![0u8; want];
                    let _ = s.read_exact(&mut buf).await;
                }
            }
        });
    }

    /// True only if BOTH the file's head (container header / track info) AND its
    /// tail (the MKV Cues / MP4 moov seek-index) are actually downloaded — the two
    /// regions a player probes before playback. A `FileStream` read blocks until
    /// the covering pieces arrive, so a short-timeout read that COMPLETES proves
    /// presence; one that times out means "not yet" (and the read keeps those
    /// pieces prioritized). Gates `/vlc/ready` so VLC never opens onto a
    /// still-downloading tail and hangs probing hundreds of MB deep.
    pub async fn ends_present(&self, id: u32, file_idx: usize) -> bool {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        const PROBE: usize = 64 * 1024;
        const T: std::time::Duration = std::time::Duration::from_millis(400);
        let Some(handle) = self.handle(id) else {
            return false;
        };
        // Tail first — the region VLC actually blocks on.
        let tail_ok = if let Ok(mut s) = handle.clone().stream(file_idx) {
            let len = s.len();
            if len <= PROBE as u64 {
                true
            } else if tokio::time::timeout(T, s.seek(std::io::SeekFrom::Start(len - PROBE as u64)))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
            {
                let mut buf = vec![0u8; PROBE];
                tokio::time::timeout(T, s.read_exact(&mut buf))
                    .await
                    .map(|r| r.is_ok())
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        };
        if !tail_ok {
            return false;
        }
        // Head — container header, served first for playback.
        if let Ok(mut s) = handle.stream(file_idx) {
            let mut buf = vec![0u8; PROBE];
            tokio::time::timeout(T, s.read_exact(&mut buf))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
        } else {
            false
        }
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
    format!(
        "http://{}:{}/torrent/{}/stream/{}",
        host, port, id, file_idx
    )
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
            (live.snapshot.peer_stats.live, bps)
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
mod settle_tests {
    use super::TorrentEngine;
    use std::io::Write;

    /// The distinction the boot-time cleanup turns on, and the one that is easy to get
    /// wrong: a torrent file is created at its FULL logical size the moment it is
    /// selected, so `len()` reports gigabytes for a file holding nothing. Deleting on
    /// `len() == 0` would never fire; deleting on "the file exists" would fire on
    /// everything. Only the block count separates a placeholder from real data.
    #[test]
    fn a_sparse_placeholder_holds_no_physical_bytes_however_large_it_claims_to_be() {
        let dir = std::env::temp_dir().join(format!("spela_phys_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let placeholder = dir.join("placeholder.mkv");
        let f = std::fs::File::create(&placeholder).unwrap();
        f.set_len(3_373_117_433).unwrap(); // the real Vyndros episode size
        drop(f);
        assert_eq!(
            std::fs::metadata(&placeholder).unwrap().len(),
            3_373_117_433,
            "it claims to be 3.14 GB"
        );
        assert_eq!(
            TorrentEngine::physical_bytes(std::iter::once(placeholder.clone())),
            0,
            "...and holds nothing, which is what decides"
        );

        let real = dir.join("real.mkv");
        let mut g = std::fs::File::create(&real).unwrap();
        g.write_all(&vec![7u8; 256 * 1024]).unwrap();
        drop(g);
        assert!(
            TorrentEngine::physical_bytes(std::iter::once(real.clone())) > 0,
            "a file with data must never be mistaken for a placeholder"
        );

        // Mixed: one real file among placeholders is enough to keep the whole torrent.
        assert!(
            TorrentEngine::physical_bytes(vec![placeholder, real].into_iter()) > 0,
            "a season pack with one downloaded episode is not empty"
        );

        // A path that does not exist contributes nothing and must not panic.
        assert_eq!(
            TorrentEngine::physical_bytes(std::iter::once(dir.join("absent.mkv"))),
            0
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The upload ceiling is a NUMBER in a config struct, which is exactly the kind of
    /// thing that can be defined, documented, and never actually wired — it happened
    /// once already in this file, where the constant existed and nothing referenced it,
    /// so the cap was reported as live while librqbit ran unlimited.
    #[test]
    fn the_upload_ceiling_is_a_hundred_megabit_expressed_in_bytes() {
        assert_eq!(super::MAX_UPLOAD_BYTES_PER_SEC, 12_500_000);
        assert_eq!(
            super::MAX_UPLOAD_BYTES_PER_SEC as u64 * 8,
            100_000_000,
            "librqbit spends this quota in BYTES despite the `_bps` name"
        );
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
            assert_ne!(
                spela_id, 0,
                "shifted id must never be the Local Bypass sentinel"
            );
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
