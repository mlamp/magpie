//! Multi-torrent engine — the public entry point consumers (lightorrent, BDD
//! tests, end-to-end harness) talk to.
//!
//! The engine owns:
//!
//! - The shared [`AlertQueue`] (created by the consumer and handed in).
//! - One [`DiskWriter`] task per torrent.
//! - One [`TorrentSession`] actor per torrent.
//! - The per-torrent [`SessionCommand`] channel used to register peers at
//!   runtime and trigger shutdown.
//!
//! Consumers add a torrent with [`Engine::add_torrent`], then attach peers
//! either by socket address ([`Engine::add_peer`]) — which goes through the
//! engine's [`PeerFilter`] — or by handing over an already-connected stream
//! ([`Engine::add_peer_stream`]) for tests and bespoke transports.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::alerts::{Alert, AlertErrorCode};
use crate::tracker::{AnnounceEvent, AnnounceRequest, Tracker};

use crate::alerts::AlertQueue;
use crate::peer_filter::{DefaultPeerFilter, PeerFilter};
use crate::session::read_cache::ReadCache;
use crate::session::shaper::{Refiller, Shaper};
use crate::session::{
    DEFAULT_PER_PEER_IN_FLIGHT, DiskMetrics, DiskWriter, HandshakeError, HandshakeRole,
    PEER_TO_SESSION_CAPACITY, PeerConfig, PeerConn, PeerSlot, PeerToSession, SessionCommand,
    TorrentParams, TorrentParamsError, TorrentSession, perform_handshake, read_handshake,
    write_handshake,
};
use crate::storage::Storage;

pub use crate::ids::TorrentId;

/// Per-peer TCP connect budget (M1 baseline). Without this, NAT'd /
/// firewalled peer addresses returned by the tracker could pin a single
/// `add_peer` task for ~75 s of OS-default connect timeout.
pub const DEFAULT_PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Returned by [`Engine::pause`], [`Engine::resume`] (G1) — and intended for
/// future per-torrent operations that look up by id — when the torrent id is
/// not registered.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("unknown torrent: {0:?}")]
pub struct TorrentNotFoundError(pub TorrentId);

/// Read-only snapshot of a live torrent's engine-level state (G3).
///
/// Returned by [`Engine::torrent_state`]. Captures fields a consumer needs to
/// reconcile its own persistent state with magpie's registry after a restart
/// — `info_hash` + `total_length` identify the torrent, the two `peer_*`
/// fields summarise occupancy. Explicitly excludes channel senders, actor
/// handles, and filter Arcs: this is a view, not a handle.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TorrentStateView {
    /// Torrent info-hash (v1 SHA-1).
    pub info_hash: [u8; 20],
    /// Total content length in bytes, as declared by the torrent params.
    pub total_length: u64,
    /// Current connected-peer count at snapshot time. Not stable — the value
    /// can change between snapshot and consumption.
    pub peer_count: usize,
    /// Per-torrent peer cap.
    pub peer_cap: usize,
}

/// Specification handed to [`Engine::add_torrent`].
pub struct AddTorrentRequest {
    /// Torrent info-hash — sent in handshakes; checked against peers.
    pub info_hash: [u8; 20],
    /// Static torrent geometry (piece count, length, hashes).
    pub params: TorrentParams,
    /// Storage backend for piece data.
    pub storage: Arc<dyn Storage>,
    /// Local 20-byte peer id sent in handshakes.
    pub peer_id: [u8; 20],
    /// SSRF-resistant filter applied to addresses passed to
    /// [`Engine::add_peer`].
    pub peer_filter: Arc<dyn PeerFilter>,
    /// Per-peer in-flight ceiling.
    pub max_in_flight: u32,
    /// Per-message wire codec ceiling.
    pub max_payload: u32,
    /// Whether to advertise BEP 6 Fast extension.
    pub fast_ext: bool,
    /// Whether to advertise BEP 10 extension protocol support.
    pub extension_protocol: bool,
    /// Handshake budget per peer.
    pub handshake_timeout: Duration,
    /// BEP 10 extension handshake timeout per peer.
    pub extension_handshake_timeout: Duration,
    /// `DiskWriter` op queue capacity (also the upper bound on in-flight
    /// unverified piece buffers).
    pub disk_queue_capacity: usize,
    /// Per-torrent connected-peer cap. Both inbound (A2) and outbound
    /// connections are counted. Once reached, further attachments are refused
    /// with [`AddPeerError::PeerCapExceeded`] (outbound) or silently dropped
    /// (inbound).
    pub peer_cap: usize,
    /// Optional initial "have" bitmap (length must equal `piece_count`).
    /// Intended for resume-from-disk and seed-mode tests: marks the listed
    /// pieces as already-verified so the seed path can serve them
    /// immediately. Empty vec means "no pre-existing pieces".
    pub initial_have: Vec<bool>,
    /// Optional raw info dict bytes. When provided, the session stores them
    /// so it can serve BEP 9 `ut_metadata` Data responses to peers.
    pub info_dict_bytes: Option<Vec<u8>>,
    /// Override the BEP 11 outbound PEX round interval. `None` keeps the
    /// 60 s default ([`crate::session::pex::PEX_INTERVAL`]). Test-only knob:
    /// integration tests that verify PEX discovery can lower this to keep
    /// runtime under a few seconds without waiting for a real-world round.
    pub pex_interval: Option<Duration>,
}

/// Default per-torrent connected-peer cap. Chosen to comfortably exceed
/// typical swarm-facing connection counts while bounding per-torrent memory.
pub const DEFAULT_PER_TORRENT_PEER_CAP: usize = 50;

/// Default global connected-peer cap across all torrents. Override via
/// [`Engine::with_global_peer_cap`].
pub const DEFAULT_GLOBAL_PEER_CAP: usize = 500;

impl AddTorrentRequest {
    /// Construct with magpie defaults — strict-no-loopback peer filter,
    /// `DEFAULT_PER_PEER_IN_FLIGHT` per peer, BEP 6 enabled, 10 s handshake,
    /// 64-deep disk queue.
    #[must_use]
    pub fn new(
        info_hash: [u8; 20],
        params: TorrentParams,
        storage: Arc<dyn Storage>,
        peer_id: [u8; 20],
    ) -> Self {
        Self {
            info_hash,
            params,
            storage,
            peer_id,
            peer_filter: Arc::new(DefaultPeerFilter::default()),
            max_in_flight: DEFAULT_PER_PEER_IN_FLIGHT,
            max_payload: magpie_bt_wire::DEFAULT_MAX_PAYLOAD,
            fast_ext: true,
            extension_protocol: true,
            handshake_timeout: Duration::from_secs(10),
            extension_handshake_timeout: crate::session::peer::DEFAULT_EXTENSION_HANDSHAKE_TIMEOUT,
            disk_queue_capacity: crate::session::DEFAULT_DISK_QUEUE_CAPACITY,
            peer_cap: DEFAULT_PER_TORRENT_PEER_CAP,
            initial_have: Vec::new(),
            info_dict_bytes: None,
            pex_interval: None,
        }
    }
}

/// Parsed magnet link fields used by [`AddMagnetRequest`].
#[derive(Debug, Clone)]
pub struct MagnetLink {
    /// v1 info-hash (20 bytes).
    pub info_hash: [u8; 20],
    /// Tracker URLs from `tr` parameters.
    pub trackers: Vec<String>,
    /// Direct peer addresses from `x.pe` parameters.
    pub peer_addrs: Vec<SocketAddr>,
    /// Display name from `dn` parameter.
    pub display_name: Option<String>,
}

/// Specification handed to [`Engine::add_magnet`].
pub struct AddMagnetRequest {
    /// The parsed magnet link.
    pub magnet: MagnetLink,
    /// Storage backend for piece data (used once metadata is known).
    pub storage: Arc<dyn Storage>,
    /// Local 20-byte peer id sent in handshakes.
    pub peer_id: [u8; 20],
    /// SSRF-resistant filter applied to addresses.
    pub peer_filter: Arc<dyn PeerFilter>,
    /// Per-peer in-flight ceiling.
    pub max_in_flight: u32,
    /// Per-message wire codec ceiling.
    pub max_payload: u32,
    /// Whether to advertise BEP 6 Fast extension.
    pub fast_ext: bool,
    /// Whether to advertise BEP 10 extension protocol support.
    pub extension_protocol: bool,
    /// Handshake budget per peer.
    pub handshake_timeout: Duration,
    /// BEP 10 extension handshake timeout per peer.
    pub extension_handshake_timeout: Duration,
    /// `DiskWriter` op queue capacity.
    pub disk_queue_capacity: usize,
    /// Per-torrent connected-peer cap.
    pub peer_cap: usize,
}

impl AddMagnetRequest {
    /// Construct with magpie defaults.
    #[must_use]
    pub fn new(magnet: MagnetLink, storage: Arc<dyn Storage>, peer_id: [u8; 20]) -> Self {
        Self {
            magnet,
            storage,
            peer_id,
            peer_filter: Arc::new(DefaultPeerFilter::default()),
            max_in_flight: DEFAULT_PER_PEER_IN_FLIGHT,
            max_payload: magpie_bt_wire::DEFAULT_MAX_PAYLOAD,
            fast_ext: true,
            extension_protocol: true,
            handshake_timeout: Duration::from_secs(10),
            extension_handshake_timeout: crate::session::peer::DEFAULT_EXTENSION_HANDSHAKE_TIMEOUT,
            disk_queue_capacity: crate::session::DEFAULT_DISK_QUEUE_CAPACITY,
            peer_cap: DEFAULT_PER_TORRENT_PEER_CAP,
        }
    }
}

/// Errors returned from [`Engine::add_magnet`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddMagnetError {
    /// The session channel was already closed.
    #[error("session closed")]
    SessionClosed,
}

/// Errors returned from [`Engine::add_torrent`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddTorrentError {
    /// [`TorrentParams`] failed validation. (E14 hardening — without this
    /// `finalise_piece` would panic on an out-of-range piece-hash slice.)
    #[error("invalid torrent params: {0}")]
    InvalidParams(#[from] TorrentParamsError),
}

/// Errors returned from [`Engine::add_peer`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddPeerError {
    /// `torrent_id` does not refer to an active torrent.
    #[error("unknown torrent id: {0:?}")]
    UnknownTorrent(TorrentId),
    /// The configured [`PeerFilter`] rejected this address.
    #[error("peer address {0} rejected by filter")]
    Filtered(SocketAddr),
    /// TCP connection failed.
    #[error("connect: {0}")]
    Connect(#[from] std::io::Error),
    /// Handshake exchange failed.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    /// The torrent's session has shut down.
    #[error("torrent session is shutting down")]
    SessionClosed,
    /// A peer with the same peer-ID is already connected to this torrent.
    /// Outbound path surfaces this after the full handshake exchange;
    /// inbound path silently drops the connection instead (plan invariant #6).
    #[error("peer-id already connected for this torrent")]
    PeerIdCollision,
    /// Connection would exceed the global or per-torrent peer cap.
    #[error("peer cap exceeded ({scope})")]
    PeerCapExceeded {
        /// Which cap was hit.
        scope: PeerCapScope,
    },
}

/// Which peer-cap limit was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerCapScope {
    /// The engine's global peer cap (see [`Engine::with_global_peer_cap`]).
    Global,
    /// This torrent's [`AddTorrentRequest::peer_cap`].
    Torrent,
}

impl std::fmt::Display for PeerCapScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Torrent => f.write_str("torrent"),
        }
    }
}

struct TorrentEntry {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    peer_filter: Arc<dyn PeerFilter>,
    peer_to_session_tx: mpsc::Sender<PeerToSession>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    disk_metrics: Arc<DiskMetrics>,
    handshake_template: PeerConfig,
    total_length: u64,
    /// Active peer-ID set for collision detection (plan invariant #6).
    /// Inbound: reject after reading peer's handshake without replying.
    /// Outbound: reject post-handshake with `AddPeerError::PeerIdCollision`.
    /// `std::sync::Mutex` is deliberate — critical sections are tiny
    /// (insert/remove of a `[u8; 20]`); never held across await.
    active_peer_ids: Arc<StdMutex<HashSet<[u8; 20]>>>,
    /// Current connected-peer count. Paired with [`TorrentEntry::peer_cap`]
    /// for per-torrent enforcement. Incremented when a peer task is spawned,
    /// decremented when it exits.
    peer_count: Arc<AtomicUsize>,
    peer_cap: usize,
    /// Storage backend, retained so [`Engine::remove`] can call
    /// [`Storage::delete`] when the consumer asks for `delete_files = true`
    /// (G2). The disk writer holds its own clone of the same `Arc`.
    storage: Arc<dyn Storage>,
    /// Per-torrent cumulative counters (ADR-0014). Live peers contribute
    /// via their own `Arc<PeerStats>` in `live_peer_stats`.
    torrent_stats: Arc<crate::session::stats::PerTorrentStats>,
    /// Live per-peer stats handles keyed by slot. Updated by
    /// `spawn_peer_task` (insert) and the peer-task exit path (retire +
    /// remove). `std::sync::Mutex` is fine — touched only on peer
    /// lifecycle boundaries, never in the hot path (peer task has its
    /// own `Arc<PeerStats>` clone).
    live_peer_stats: Arc<StdMutex<HashMap<PeerSlot, Arc<crate::session::stats::PeerStats>>>>,
}

fn reserve_peer_id(set: &StdMutex<HashSet<[u8; 20]>>, peer_id: [u8; 20]) -> bool {
    let mut guard = set.lock().expect("active_peer_ids poisoned");
    guard.insert(peer_id)
}

fn release_peer_id(set: &StdMutex<HashSet<[u8; 20]>>, peer_id: [u8; 20]) {
    let mut guard = set.lock().expect("active_peer_ids poisoned");
    guard.remove(&peer_id);
}

/// Try to reserve a slot against both the engine-global and torrent-local
/// peer caps. Fetch-add + rollback is race-free for our purposes (a concurrent
/// attempt may briefly overshoot the cap by a few, acceptable under
/// contention). On `Ok`, callers **must** call [`release_peer_slot`] exactly
/// once when the peer task exits.
fn reserve_peer_slot(
    global_count: &AtomicUsize,
    global_cap: usize,
    torrent_count: &AtomicUsize,
    torrent_cap: usize,
) -> Result<(), PeerCapScope> {
    let global_prev = global_count.fetch_add(1, Ordering::Acquire);
    if global_prev >= global_cap {
        global_count.fetch_sub(1, Ordering::Release);
        return Err(PeerCapScope::Global);
    }
    let local_prev = torrent_count.fetch_add(1, Ordering::Acquire);
    if local_prev >= torrent_cap {
        torrent_count.fetch_sub(1, Ordering::Release);
        global_count.fetch_sub(1, Ordering::Release);
        return Err(PeerCapScope::Torrent);
    }
    Ok(())
}

fn release_peer_slot(global_count: &AtomicUsize, torrent_count: &AtomicUsize) {
    torrent_count.fetch_sub(1, Ordering::Release);
    global_count.fetch_sub(1, Ordering::Release);
}

/// Configuration for [`Engine::attach_tracker`].
#[derive(Debug, Clone, Copy)]
pub struct AttachTrackerConfig {
    /// Port to advertise in announce requests. Magpie has no listener of its
    /// own in M1 (leecher only), so this is informational for trackers that
    /// echo it back to other peers.
    pub listen_port: u16,
    /// Hint passed to the tracker as `numwant`. `None` lets the tracker pick.
    pub num_want: Option<u32>,
    /// Time to back off after a tracker error before retrying.
    pub error_backoff: Duration,
}

impl Default for AttachTrackerConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            num_want: Some(50),
            error_backoff: Duration::from_mins(1),
        }
    }
}

/// The multi-torrent engine.
///
/// **Lifecycle (E13)**: dropping `Engine` does not gracefully stop running
/// torrents — spawned tasks become tokio-runtime orphans and are aborted when
/// the runtime shuts down. For clean teardown, call
/// [`Engine::shutdown`] for every torrent then [`Engine::join`] before
/// dropping.
pub struct Engine {
    pub(crate) alerts: Arc<AlertQueue>,
    next_torrent_id: AtomicU64,
    next_peer_slot: AtomicU64,
    // E1: RwLock so concurrent `add_peer` / `disk_metrics` reads don't
    // serialise behind `add_torrent` / `shutdown` writes.
    torrents: RwLock<HashMap<TorrentId, TorrentEntry>>,
    /// `info_hash` → [`TorrentId`] index for inbound connection routing (A2).
    /// Mirrors `torrents` and is updated under the same write lock to stay
    /// consistent.
    info_hash_index: RwLock<HashMap<[u8; 20], TorrentId>>,
    /// Current connected-peer count across all torrents. Enforced against
    /// `global_peer_cap` in `spawn_peer_task`. `Arc` so peer tasks can
    /// decrement it when they exit, even if the engine is being torn down
    /// concurrently.
    global_peer_count: Arc<AtomicUsize>,
    global_peer_cap: usize,
    /// Session-global read cache shared across all torrents (ADR-0018).
    read_cache: Arc<ReadCache>,
    /// Three-tier bandwidth shaper (ADR-0013). Peer tasks consume from
    /// their per-peer bucket on the hot path (send/recv); torrent and
    /// session tiers are refreshed by the `Refiller` via demand
    /// aggregation (see `#22` plan invariant).
    shaper: Arc<Shaper>,
    /// Long-running Refiller task. Aborted in [`Self::join`] so callers
    /// awaiting join don't hang on the infinite refill loop.
    refiller_task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Cancellation token signalled by [`Self::join`]. The listener accept
    /// loop selects on a child of this token so it exits gracefully, giving
    /// peer tasks time to run cleanup before being aborted.
    shutdown_token: CancellationToken,
    /// Bound listen port (BEP 10 `p` field source). Set by [`Self::listen`];
    /// read in [`Self::attach_stream`] so every outbound and inbound
    /// extension handshake we send carries the reachable port other peers
    /// should dial. `0` means "no listener bound" — the field is omitted
    /// from the handshake.
    listen_port: AtomicU16,
    /// Engine-global file-descriptor pool for [`MultiFileStorage`](
    /// crate::storage::MultiFileStorage) instances. Consumers that construct
    /// their own storage can share this pool via [`Self::fd_pool`] so all
    /// torrents in one engine compete for one bounded fd budget, matching
    /// rakshasa's `FileManager` scope. Unix-only; `None` on other platforms.
    #[cfg(unix)]
    fd_pool: Arc<crate::storage::FdPool>,
}

impl Engine {
    /// Construct an engine that will publish events to `alerts`. Uses
    /// [`DEFAULT_GLOBAL_PEER_CAP`] across all torrents; override with
    /// [`Engine::with_global_peer_cap`].
    #[must_use]
    pub fn new(alerts: Arc<AlertQueue>) -> Self {
        let shaper = Arc::new(Shaper::new());
        let refiller = Refiller::new(Arc::clone(&shaper));
        let refiller_task = tokio::spawn(refiller.run());
        Self {
            alerts,
            next_torrent_id: AtomicU64::new(1), // 0 reserved
            next_peer_slot: AtomicU64::new(0),
            torrents: RwLock::new(HashMap::new()),
            info_hash_index: RwLock::new(HashMap::new()),
            global_peer_count: Arc::new(AtomicUsize::new(0)),
            global_peer_cap: DEFAULT_GLOBAL_PEER_CAP,
            read_cache: Arc::new(ReadCache::with_defaults()),
            shaper,
            refiller_task: Mutex::new(Some(refiller_task)),
            tasks: Mutex::new(Vec::new()),
            shutdown_token: CancellationToken::new(),
            listen_port: AtomicU16::new(0),
            #[cfg(unix)]
            fd_pool: Arc::new(crate::storage::FdPool::with_default_cap()),
        }
    }

    /// Override the engine-global [`FdPool`](crate::storage::FdPool) cap.
    /// Default is [`FdPool::default_cap`](crate::storage::FdPool::default_cap)
    /// (128). Call before adding any torrents whose storage you'll derive
    /// from [`Self::fd_pool`]; existing storages keep the pool they were
    /// constructed with.
    #[cfg(unix)]
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn with_fd_pool_cap(mut self, cap: usize) -> Self {
        self.fd_pool = Arc::new(crate::storage::FdPool::with_cap(cap));
        self
    }

    /// Engine-global fd pool — hand this to
    /// [`MultiFileStorage::create_from_info`](
    /// crate::storage::MultiFileStorage::create_from_info) so all torrents
    /// added to this engine share one fd budget. Consumers that want an
    /// isolated pool per torrent can construct their own
    /// [`FdPool`](crate::storage::FdPool) instead.
    #[cfg(unix)]
    #[must_use]
    pub fn fd_pool(&self) -> Arc<crate::storage::FdPool> {
        Arc::clone(&self.fd_pool)
    }

    /// Override the global connected-peer cap. Default is
    /// [`DEFAULT_GLOBAL_PEER_CAP`].
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Self carries Arc<...>, not const-compatible
    pub fn with_global_peer_cap(mut self, cap: usize) -> Self {
        self.global_peer_cap = cap;
        self
    }

    /// Shared alert queue.
    #[must_use]
    pub fn alerts(&self) -> Arc<AlertQueue> {
        Arc::clone(&self.alerts)
    }

    /// Three-tier bandwidth shaper (ADR-0013). Exposed so tests and
    /// consumers can pin per-peer or per-torrent rates at runtime via
    /// `shaper.peer_buckets(slot).unwrap().up.set_rate_bps(...)`.
    #[must_use]
    pub fn shaper(&self) -> Arc<Shaper> {
        Arc::clone(&self.shaper)
    }

    /// Cumulative per-torrent stats snapshot (ADR-0014). Sums live
    /// per-peer counters with the retired-peer accumulator. Returns
    /// `None` if the torrent is not registered.
    ///
    /// # Panics
    ///
    /// Panics only if the internal `live_peer_stats` mutex is poisoned
    /// (unrecoverable).
    #[must_use]
    pub async fn torrent_stats_snapshot(
        &self,
        torrent_id: TorrentId,
    ) -> Option<crate::session::stats::StatsSnapshot> {
        let guard = self.torrents.read().await;
        let entry = guard.get(&torrent_id)?;
        let info_hash = entry.info_hash;
        let torrent_stats = Arc::clone(&entry.torrent_stats);
        let live = Arc::clone(&entry.live_peer_stats);
        drop(guard);
        let live_peers = live.lock().expect("live_peer_stats poisoned");
        let refs: Vec<_> = live_peers.values().map(AsRef::as_ref).collect();
        let (uploaded, downloaded) = torrent_stats.snapshot(refs);
        drop(live_peers);
        Some(crate::session::stats::StatsSnapshot {
            info_hash,
            uploaded,
            downloaded,
        })
    }

    /// Add a torrent; spawns its [`DiskWriter`] and [`TorrentSession`] tasks.
    ///
    /// # Errors
    ///
    /// Returns [`AddTorrentError::InvalidParams`] if `req.params` violates the
    /// invariants enforced by [`TorrentParams::validate`].
    pub async fn add_torrent(&self, req: AddTorrentRequest) -> Result<TorrentId, AddTorrentError> {
        req.params.validate()?;
        tracing::info!(
            piece_count = req.params.piece_count,
            total_length = req.params.total_length,
            "engine: adding torrent",
        );
        let total_length = req.params.total_length;
        let (disk_writer, disk_tx, disk_metrics) =
            DiskWriter::new(Arc::clone(&req.storage), req.disk_queue_capacity);
        let disk_task = tokio::spawn(disk_writer.run());

        let id = TorrentId::new(self.next_torrent_id.fetch_add(1, Ordering::Relaxed));

        let (peer_to_session_tx, peer_to_session_rx) = mpsc::channel(PEER_TO_SESSION_CAPACITY);
        let (mut session, cmd_tx) = TorrentSession::new(
            id,
            req.params,
            req.info_hash,
            Arc::clone(&self.alerts),
            peer_to_session_rx,
            disk_tx,
            Arc::clone(&self.read_cache),
            req.peer_cap,
        );
        if !req.initial_have.is_empty() {
            session.apply_initial_have(&req.initial_have);
        }
        let metadata_size = req.info_dict_bytes.as_ref().map(|b| b.len() as u64);
        if let Some(info_bytes) = req.info_dict_bytes {
            session.set_info_dict_bytes(info_bytes);
        }
        if let Some(interval) = req.pex_interval {
            session.set_pex_interval(interval);
        }

        let handshake_template = PeerConfig {
            peer_id: req.peer_id,
            info_hash: req.info_hash,
            fast_ext: req.fast_ext,
            extension_protocol: req.extension_protocol,
            max_in_flight: req.max_in_flight,
            max_payload: req.max_payload,
            handshake_timeout: req.handshake_timeout,
            extension_handshake_timeout: req.extension_handshake_timeout,
            remote_addr: None,
            metadata_size,
            local_listen_port: None, // stamped per-attach in attach_stream
        };
        let entry = TorrentEntry {
            info_hash: req.info_hash,
            peer_id: req.peer_id,
            peer_filter: req.peer_filter,
            peer_to_session_tx,
            cmd_tx,
            disk_metrics,
            handshake_template,
            total_length,
            active_peer_ids: Arc::new(StdMutex::new(HashSet::new())),
            peer_count: Arc::new(AtomicUsize::new(0)),
            peer_cap: req.peer_cap,
            storage: Arc::clone(&req.storage),
            torrent_stats: Arc::new(crate::session::stats::PerTorrentStats::new()),
            live_peer_stats: Arc::new(StdMutex::new(HashMap::new())),
        };
        // Insert into the registry BEFORE spawning the session task so that
        // alerts referencing this TorrentId are never delivered before the
        // consumer can look the torrent up.
        let mut torrents = self.torrents.write().await;
        let mut index = self.info_hash_index.write().await;
        torrents.insert(id, entry);
        index.insert(req.info_hash, id);
        drop(index);
        drop(torrents);
        // Register the torrent tier at pass-through by default. ADR-0013
        // #22 plan invariant: hot path consumes peer tier only; this tier
        // only participates in refill demand aggregation until M5 cap
        // enablement flips rates off passthrough.
        self.shaper.register_torrent_passthrough(id);

        let session_task = tokio::spawn(async move {
            let _ = session.run().await;
        });
        self.tasks.lock().await.push(disk_task);
        self.tasks.lock().await.push(session_task);
        Ok(id)
    }

    /// Start a torrent from a magnet link. Returns a [`TorrentId`]
    /// immediately; the metadata exchange runs asynchronously. When the info
    /// dict has been downloaded and verified, an
    /// [`Alert::MetadataReceived`] is emitted and the torrent transitions
    /// to normal downloading.
    ///
    /// # Errors
    ///
    /// Returns [`AddMagnetError::SessionClosed`] if the session fails to
    /// start (should not happen in practice).
    pub async fn add_magnet(&self, req: AddMagnetRequest) -> Result<TorrentId, AddMagnetError> {
        let info_hash = req.magnet.info_hash;
        tracing::info!(?info_hash, "engine: adding magnet");

        // Create a minimal "placeholder" TorrentParams. The real params will
        // be set once metadata is downloaded. Use 1 piece to satisfy the
        // session's constructor — the assembler overrides this on completion.
        let placeholder_params = TorrentParams {
            piece_count: 1,
            piece_length: 16 * 1024,
            total_length: 16 * 1024,
            piece_hashes: vec![0u8; 20],
            private: false,
        };

        let (disk_writer, disk_tx, disk_metrics) =
            DiskWriter::new(Arc::clone(&req.storage), req.disk_queue_capacity);
        let disk_task = tokio::spawn(disk_writer.run());

        let id = TorrentId::new(self.next_torrent_id.fetch_add(1, Ordering::Relaxed));

        let (peer_to_session_tx, peer_to_session_rx) = mpsc::channel(PEER_TO_SESSION_CAPACITY);
        let (mut session, cmd_tx) = TorrentSession::new(
            id,
            placeholder_params,
            info_hash,
            Arc::clone(&self.alerts),
            peer_to_session_rx,
            disk_tx,
            Arc::clone(&self.read_cache),
            req.peer_cap,
        );
        // Attach the metadata assembler — this puts the session in
        // "metadata-fetching" mode.
        session.set_metadata_assembler(crate::session::metadata_exchange::MetadataAssembler::new(
            info_hash,
        ));

        let handshake_template = PeerConfig {
            peer_id: req.peer_id,
            info_hash,
            fast_ext: req.fast_ext,
            extension_protocol: req.extension_protocol,
            max_in_flight: req.max_in_flight,
            max_payload: req.max_payload,
            handshake_timeout: req.handshake_timeout,
            extension_handshake_timeout: req.extension_handshake_timeout,
            remote_addr: None,
            metadata_size: None,     // unknown until metadata arrives
            local_listen_port: None, // stamped per-attach in attach_stream
        };
        let entry = TorrentEntry {
            info_hash,
            peer_id: req.peer_id,
            peer_filter: req.peer_filter,
            peer_to_session_tx,
            cmd_tx,
            disk_metrics,
            handshake_template,
            total_length: 0, // unknown until metadata arrives
            active_peer_ids: Arc::new(StdMutex::new(HashSet::new())),
            peer_count: Arc::new(AtomicUsize::new(0)),
            peer_cap: req.peer_cap,
            storage: Arc::clone(&req.storage),
            torrent_stats: Arc::new(crate::session::stats::PerTorrentStats::new()),
            live_peer_stats: Arc::new(StdMutex::new(HashMap::new())),
        };

        let mut torrents = self.torrents.write().await;
        let mut index = self.info_hash_index.write().await;
        torrents.insert(id, entry);
        index.insert(info_hash, id);
        drop(index);
        drop(torrents);
        self.shaper.register_torrent_passthrough(id);

        let session_task = tokio::spawn(async move {
            let _ = session.run().await;
        });
        self.tasks.lock().await.push(disk_task);
        self.tasks.lock().await.push(session_task);
        Ok(id)
    }

    /// Connect to `addr`, perform the BEP 3 handshake, and attach the peer
    /// task to the named torrent.
    pub async fn add_peer(
        &self,
        torrent_id: TorrentId,
        addr: SocketAddr,
    ) -> Result<(), AddPeerError> {
        let snapshot = self.snapshot(torrent_id).await?;
        if !snapshot.peer_filter.allow(addr) {
            tracing::debug!(%addr, "peer filtered");
            return Err(AddPeerError::Filtered(addr));
        }
        tracing::debug!(%addr, "engine: connecting to peer");
        let stream = tokio::time::timeout(DEFAULT_PEER_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                AddPeerError::Connect(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("tcp connect to {addr} timed out"),
                ))
            })??;
        self.attach_stream(snapshot, stream, HandshakeRole::Initiator, Some(addr))
            .await
    }

    /// Attach a peer using a caller-supplied stream that hasn't been
    /// handshaken yet, identified by `peer_addr`. The configured
    /// [`PeerFilter`] is applied to `peer_addr` exactly as it is by
    /// [`Engine::add_peer`] (E16 hardening: the previous `add_peer_stream`
    /// signature took only the stream and bypassed the filter).
    ///
    /// Useful for accepting inbound connections from a `TcpListener` you own,
    /// or for test transports built on [`tokio::io::duplex`]. For the latter
    /// case use a synthetic address (e.g. `192.0.2.1:6881` from the
    /// documentation prefix) and configure
    /// [`DefaultPeerFilter::permissive_for_tests`].
    pub async fn add_peer_stream<S>(
        &self,
        torrent_id: TorrentId,
        peer_addr: SocketAddr,
        stream: S,
        role: HandshakeRole,
    ) -> Result<(), AddPeerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let snapshot = self.snapshot(torrent_id).await?;
        if !snapshot.peer_filter.allow(peer_addr) {
            return Err(AddPeerError::Filtered(peer_addr));
        }
        self.attach_stream(snapshot, stream, role, Some(peer_addr))
            .await
    }

    /// Disk metrics handle for the named torrent.
    pub async fn disk_metrics(&self, torrent_id: TorrentId) -> Option<Arc<DiskMetrics>> {
        let guard = self.torrents.read().await;
        let metrics = guard.get(&torrent_id).map(|e| Arc::clone(&e.disk_metrics));
        drop(guard);
        metrics
    }

    /// Pause a torrent: peers stay connected but get choked, and no new
    /// piece requests are scheduled. Idempotent — pausing an already-paused
    /// torrent is a no-op. G1 per `docs/api-audit.md`.
    ///
    /// # Errors
    ///
    /// [`TorrentNotFoundError`] if `torrent_id` is not registered. A torrent
    /// removed by [`Engine::shutdown`] returns this immediately.
    pub async fn pause(&self, torrent_id: TorrentId) -> Result<(), TorrentNotFoundError> {
        let cmd_tx = {
            let guard = self.torrents.read().await;
            guard
                .get(&torrent_id)
                .map(|e| e.cmd_tx.clone())
                .ok_or(TorrentNotFoundError(torrent_id))?
        };
        // Send-failure means the actor is already gone; treat as
        // not-registered rather than surface a separate channel-closed
        // variant — the consumer's correct response is identical.
        cmd_tx
            .send(SessionCommand::Pause)
            .await
            .map_err(|_| TorrentNotFoundError(torrent_id))
    }

    /// Resume a previously-paused torrent. Idempotent. G1 per
    /// `docs/api-audit.md`.
    ///
    /// # Errors
    ///
    /// [`TorrentNotFoundError`] if `torrent_id` is not registered.
    pub async fn resume(&self, torrent_id: TorrentId) -> Result<(), TorrentNotFoundError> {
        let cmd_tx = {
            let guard = self.torrents.read().await;
            guard
                .get(&torrent_id)
                .map(|e| e.cmd_tx.clone())
                .ok_or(TorrentNotFoundError(torrent_id))?
        };
        cmd_tx
            .send(SessionCommand::Resume)
            .await
            .map_err(|_| TorrentNotFoundError(torrent_id))
    }

    /// List the ids of every currently-registered torrent. Consumers use this
    /// on restart to reconcile their persistent state with magpie's live
    /// registry without mirroring the registry themselves. Order is
    /// unspecified (`HashMap` iteration).
    ///
    /// G3 per `docs/api-audit.md`.
    pub async fn torrents(&self) -> Vec<TorrentId> {
        self.torrents.read().await.keys().copied().collect()
    }

    /// Read-only snapshot of a torrent's engine-level state. Returns `None`
    /// if `torrent_id` is not registered.
    ///
    /// The returned [`TorrentStateView`] is a value snapshot: the peer count
    /// may change between snapshot and consumption. Intended for display /
    /// reconciliation, not for lock-based coordination.
    ///
    /// G3 per `docs/api-audit.md`.
    pub async fn torrent_state(&self, torrent_id: TorrentId) -> Option<TorrentStateView> {
        let guard = self.torrents.read().await;
        let view = guard.get(&torrent_id).map(|entry| TorrentStateView {
            info_hash: entry.info_hash,
            total_length: entry.total_length,
            peer_count: entry.peer_count.load(Ordering::Relaxed),
            peer_cap: entry.peer_cap,
        });
        drop(guard);
        view
    }

    /// Snapshot the torrent's current verified-piece bitfield. Returns
    /// `None` if `torrent_id` is not registered or the session is gone.
    ///
    /// Consumers that persist resume state (ADR-0022) poll this on a
    /// timer (or after `Alert::PieceCompleted`), feed the result into a
    /// [`ResumeSnapshot`](crate::session::resume::ResumeSnapshot), and
    /// hand it to their
    /// [`ResumeSink`](crate::session::resume::ResumeSink).
    ///
    /// The returned `Vec<bool>` is a cloned snapshot: mutations in the
    /// actor do not affect it, and it does not lock the actor for the
    /// duration of the caller's work.
    pub async fn torrent_bitfield_snapshot(&self, torrent_id: TorrentId) -> Option<Vec<bool>> {
        let cmd_tx = {
            let guard = self.torrents.read().await;
            let cmd_tx = guard.get(&torrent_id).map(|entry| entry.cmd_tx.clone())?;
            drop(guard);
            cmd_tx
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if cmd_tx
            .send(SessionCommand::BitfieldSnapshot { reply: reply_tx })
            .await
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    /// Drain the per-torrent buffer of PEX-discovered peer addresses
    /// (BEP 11). The session accumulates addresses learnt from inbound
    /// `ut_pex` messages; consumers (or the M3 PEX integration test)
    /// call this periodically and feed the survivors into
    /// [`Engine::add_peer`].
    ///
    /// Returns an empty vector when the torrent is private (PEX disabled),
    /// when no new peers have been discovered since the last drain, or when
    /// `torrent_id` is not registered. Channel-closed (session torn down)
    /// also collapses to an empty vector — the caller's correct response
    /// is the same as "nothing new".
    pub async fn drain_pex_discovered(&self, torrent_id: TorrentId) -> Vec<SocketAddr> {
        let cmd_tx = {
            let guard = self.torrents.read().await;
            let Some(cmd_tx) = guard.get(&torrent_id).map(|entry| entry.cmd_tx.clone()) else {
                return Vec::new();
            };
            drop(guard);
            cmd_tx
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if cmd_tx
            .send(SessionCommand::DrainPexDiscovered { reply: reply_tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Spawn a periodic announce loop against `tracker`.
    ///
    /// On every successful announce, the loop applies the torrent's configured
    /// [`PeerFilter`] to each returned peer and feeds the survivors into
    /// [`Engine::add_peer`]. The loop sleeps for
    /// [`AnnounceResponse::clamped_interval`](crate::tracker::AnnounceResponse::clamped_interval)
    /// (floored at 30 s) between announces.
    ///
    /// E17 hardening: this is the *only* way magpie consumes tracker peers
    /// internally. Bypassing it (calling `add_peer` from a manual announce
    /// loop) is permitted but the SSRF defence becomes the consumer's
    /// responsibility.
    ///
    /// # Errors
    ///
    /// Returns [`AddPeerError::UnknownTorrent`] if `torrent_id` is unknown.
    /// Per-announce failures are surfaced via the alert ring as
    /// `Alert::Error { code: TrackerFailed }`; the loop backs off and retries.
    pub async fn attach_tracker(
        self: &Arc<Self>,
        torrent_id: TorrentId,
        tracker: Arc<dyn Tracker>,
        cfg: AttachTrackerConfig,
    ) -> Result<(), AddPeerError> {
        let snapshot = self.snapshot(torrent_id).await?;
        let metrics = self
            .disk_metrics(torrent_id)
            .await
            .ok_or(AddPeerError::UnknownTorrent(torrent_id))?;
        let total_length = {
            let guard = self.torrents.read().await;
            let total = guard.get(&torrent_id).map(|e| e.total_length);
            drop(guard);
            total.ok_or(AddPeerError::UnknownTorrent(torrent_id))?
        };
        let engine = Arc::clone(self);
        let alerts = Arc::clone(&engine.alerts);
        let task = tokio::spawn(async move {
            let mut event = AnnounceEvent::Started;
            loop {
                let downloaded = metrics.bytes_written.load(Ordering::Relaxed);
                let left = total_length.saturating_sub(downloaded);
                let req = AnnounceRequest {
                    info_hash: snapshot.info_hash,
                    peer_id: snapshot.peer_id,
                    port: cfg.listen_port,
                    uploaded: 0,
                    downloaded,
                    left,
                    event,
                    num_want: cfg.num_want,
                    compact: true,
                    tracker_id: None,
                };
                match tracker.announce(req).await {
                    Ok(resp) => {
                        let peer_count = resp.peers.len();
                        let interval = resp.clamped_interval();
                        tracing::info!(
                            peers = peer_count,
                            interval_secs = resp.interval.as_secs(),
                            "tracker announce ok",
                        );
                        // Fan out connects in parallel — serial add_peer would
                        // wait through 50 × 5 s = 250 s of timeouts back-to-back
                        // on a typical NAT'd swarm.
                        for addr in resp.peers {
                            let engine = Arc::clone(&engine);
                            let alerts = Arc::clone(&alerts);
                            tokio::spawn(async move {
                                if let Err(e) = engine.add_peer(torrent_id, addr).await
                                    && !matches!(e, AddPeerError::Filtered(_))
                                {
                                    tracing::debug!(%addr, error = %e, "add_peer failed");
                                    alerts.push(Alert::Error {
                                        torrent: torrent_id,
                                        code: AlertErrorCode::PeerProtocol,
                                    });
                                }
                            });
                        }
                        event = AnnounceEvent::Periodic;
                        tokio::time::sleep(interval).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "tracker announce failed");
                        alerts.push(Alert::Error {
                            torrent: torrent_id,
                            code: AlertErrorCode::TrackerFailed,
                        });
                        tokio::time::sleep(cfg.error_backoff).await;
                    }
                }
            }
        });
        self.tasks.lock().await.push(task);
        Ok(())
    }

    /// Remove a torrent from the registry and (optionally) delete its
    /// backing storage. G2 per `docs/api-audit.md`.
    ///
    /// Distinguishes the two needs every realistic client has — *stop this
    /// torrent* (`delete_files = false`, equivalent to [`Engine::shutdown`])
    /// vs *stop and erase the data* (`delete_files = true`, calls
    /// [`Storage::delete`] after shutdown).
    ///
    /// On `delete_files = true` the storage backend's `delete` is invoked
    /// after the torrent is removed from the registry and the actor has
    /// been signalled to shut down. If the storage delete fails, an
    /// [`Alert::Error`] is emitted and the function still returns `Ok(())`
    /// — the torrent is gone from magpie's view; storage cleanup is
    /// best-effort. The consumer can fall back to a manual delete via the
    /// path it constructed the storage with.
    ///
    /// **Path safety**: magpie does not derive paths from torrent metainfo;
    /// the storage backend is constructed by the consumer with a
    /// fully-resolved path, so there is no in-magpie path-traversal
    /// surface. See [`Storage::delete`] for the trait-level note.
    ///
    /// # Errors
    ///
    /// [`TorrentNotFoundError`] if `torrent_id` is not registered.
    pub async fn remove(
        &self,
        torrent_id: TorrentId,
        delete_files: bool,
    ) -> Result<(), TorrentNotFoundError> {
        let mut torrents = self.torrents.write().await;
        let entry = torrents
            .remove(&torrent_id)
            .ok_or(TorrentNotFoundError(torrent_id))?;
        let mut index = self.info_hash_index.write().await;
        index.remove(&entry.info_hash);
        drop(index);
        drop(torrents);
        let _ = entry.cmd_tx.send(SessionCommand::Shutdown).await;
        // Release the torrent's shaper tier and any peer buckets still
        // keyed to it. Caches inside peer tasks continue to work through
        // teardown because they hold their own Arc<TokenBucket>.
        self.shaper.drop_torrent(torrent_id);
        if delete_files && let Err(err) = entry.storage.delete() {
            tracing::warn!(error = %err, ?torrent_id, "remove: storage.delete() failed");
            self.alerts.push(Alert::Error {
                torrent: torrent_id,
                code: AlertErrorCode::StorageIo,
            });
        }
        Ok(())
    }

    /// Issue a graceful shutdown on the named torrent. Returns immediately;
    /// pending peer tasks unwind on their own. Use [`Engine::join`] to wait
    /// for completion.
    pub async fn shutdown(&self, torrent_id: TorrentId) {
        let mut torrents = self.torrents.write().await;
        let mut index = self.info_hash_index.write().await;
        let entry = torrents.remove(&torrent_id);
        if let Some(ref e) = entry {
            index.remove(&e.info_hash);
        }
        drop(index);
        drop(torrents);
        if let Some(entry) = entry {
            let _ = entry.cmd_tx.send(SessionCommand::Shutdown).await;
        }
        // Release the shaper's torrent tier + any peer buckets keyed to
        // it. Idempotent if the torrent wasn't registered.
        self.shaper.drop_torrent(torrent_id);
    }

    /// Wait for every spawned task to complete. Call after shutting down all
    /// torrents. Idempotent.
    ///
    /// Signals the shutdown token so the listener exits its accept loop
    /// gracefully, then awaits all tasks with a grace period. Peer tasks
    /// that exit normally run their cleanup path (`retire_peer`,
    /// `release_peer_id`, `release_peer_slot`), preventing permanent
    /// `global_peer_count` inflation. Tasks that don't finish within the
    /// grace period are aborted as a last resort.
    pub async fn join(&self) {
        self.shutdown_token.cancel();

        let refiller_handle = self.refiller_task.lock().await.take();
        if let Some(handle) = refiller_handle {
            handle.abort();
            let _ = handle.await;
        }

        let tasks = std::mem::take(&mut *self.tasks.lock().await);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if tasks.iter().all(JoinHandle::is_finished) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        for h in &tasks {
            if !h.is_finished() {
                h.abort();
            }
        }
        for h in tasks {
            let _ = h.await;
        }
    }

    async fn snapshot(&self, torrent_id: TorrentId) -> Result<Snapshot, AddPeerError> {
        let guard = self.torrents.read().await;
        let snap = guard
            .get(&torrent_id)
            .map(|entry| Snapshot {
                cfg: entry.handshake_template.clone(),
                peer_to_session_tx: entry.peer_to_session_tx.clone(),
                cmd_tx: entry.cmd_tx.clone(),
                peer_filter: Arc::clone(&entry.peer_filter),
                info_hash: entry.info_hash,
                peer_id: entry.peer_id,
                active_peer_ids: Arc::clone(&entry.active_peer_ids),
                peer_count: Arc::clone(&entry.peer_count),
                peer_cap: entry.peer_cap,
                torrent_id,
                torrent_stats: Arc::clone(&entry.torrent_stats),
                live_peer_stats: Arc::clone(&entry.live_peer_stats),
            })
            .ok_or(AddPeerError::UnknownTorrent(torrent_id));
        drop(guard);
        snap
    }

    async fn snapshot_by_info_hash(&self, info_hash: [u8; 20]) -> Option<Snapshot> {
        let index = self.info_hash_index.read().await;
        let torrent_id = index.get(&info_hash).copied()?;
        drop(index);
        self.snapshot(torrent_id).await.ok()
    }

    async fn attach_stream<S>(
        &self,
        mut snapshot: Snapshot,
        mut stream: S,
        role: HandshakeRole,
        peer_addr: Option<SocketAddr>,
    ) -> Result<(), AddPeerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let _ = (snapshot.info_hash, snapshot.peer_id); // available for future per-handshake overrides
        // Set the remote address on the config so PeerConn can pass it
        // through in PeerToSession::Connected for PEX.
        snapshot.cfg.remote_addr = peer_addr;
        // BEP 10 `p`: stamp our listen port (if bound) into the outbound
        // extension handshake. Lets the remote rewrite our PEX address to
        // (our_ip, our_listen_port) instead of our outbound source port.
        let port = self.listen_port.load(Ordering::Relaxed);
        snapshot.cfg.local_listen_port = (port != 0).then_some(port);
        // Reserve the cap slot *before* the handshake exchange so an over-cap
        // outbound attempt fails fast without sending our handshake bytes.
        reserve_peer_slot(
            &self.global_peer_count,
            self.global_peer_cap,
            &snapshot.peer_count,
            snapshot.peer_cap,
        )
        .map_err(|scope| AddPeerError::PeerCapExceeded { scope })?;
        let remote = match perform_handshake(&mut stream, &snapshot.cfg, role).await {
            Ok(r) => r,
            Err(e) => {
                release_peer_slot(&self.global_peer_count, &snapshot.peer_count);
                return Err(e.into());
            }
        };
        // Outbound collision: we've already sent our handshake, so surface
        // the error to the caller. Inbound uses `handle_inbound` which
        // silent-drops before replying.
        if !reserve_peer_id(&snapshot.active_peer_ids, remote.peer_id) {
            release_peer_slot(&self.global_peer_count, &snapshot.peer_count);
            return Err(AddPeerError::PeerIdCollision);
        }
        self.spawn_peer_task(snapshot, stream, remote).await
    }

    async fn spawn_peer_task<S>(
        &self,
        snapshot: Snapshot,
        stream: S,
        remote: magpie_bt_wire::Handshake,
    ) -> Result<(), AddPeerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let slot = PeerSlot(self.next_peer_slot.fetch_add(1, Ordering::Relaxed));
        let (session_to_peer_tx, session_to_peer_rx) = mpsc::unbounded_channel();
        // Caller has already reserved the peer-ID and the (global + torrent)
        // peer slot. The spawned task is responsible for releasing both on
        // exit; SessionClosed failure releases them here instead.
        if snapshot
            .cmd_tx
            .send(SessionCommand::RegisterPeer {
                slot,
                tx: session_to_peer_tx,
                max_in_flight: snapshot.cfg.max_in_flight,
                // Negotiated fast-ext: AND our config with the peer's
                // advertised bit. Drives the HaveAll/HaveNone shortcut
                // in `register_peer_with` (#21 plan).
                supports_fast: snapshot.cfg.fast_ext && remote.supports_fast_ext(),
            })
            .await
            .is_err()
        {
            release_peer_id(&snapshot.active_peer_ids, remote.peer_id);
            release_peer_slot(&self.global_peer_count, &snapshot.peer_count);
            return Err(AddPeerError::SessionClosed);
        }
        // Register the peer in the shaper (pass-through at M2; rate caps
        // flip on at M5). Cache the Arc<DuplexBuckets> handle and hand it
        // to PeerConn so the hot path is lock-free.
        self.shaper.register_peer(
            slot,
            snapshot.torrent_id,
            crate::session::shaper::DuplexBuckets::passthrough(),
        );
        let peer_buckets = self
            .shaper
            .peer_buckets(slot)
            .expect("peer registered one line up");
        // Stats plumbing (ADR-0014). Allocate a PeerStats, keep one Arc
        // in the torrent's live-set, hand another to the PeerConn so
        // increments are lock-free.
        let peer_stats = Arc::new(crate::session::stats::PeerStats::new());
        snapshot
            .live_peer_stats
            .lock()
            .expect("live_peer_stats poisoned")
            .insert(slot, Arc::clone(&peer_stats));
        let conn = PeerConn::with_shaper(
            stream,
            slot,
            snapshot.cfg.clone(),
            snapshot.peer_to_session_tx,
            session_to_peer_rx,
            peer_buckets,
        )
        .with_peer_stats(Arc::clone(&peer_stats));
        let active_peer_ids = Arc::clone(&snapshot.active_peer_ids);
        let torrent_peer_count = Arc::clone(&snapshot.peer_count);
        let global_peer_count = Arc::clone(&self.global_peer_count);
        let peer_id = remote.peer_id;
        let shaper = Arc::clone(&self.shaper);
        let torrent_stats = Arc::clone(&snapshot.torrent_stats);
        let live_peer_stats = Arc::clone(&snapshot.live_peer_stats);
        let task = tokio::spawn(async move {
            conn.run(remote).await;
            // Plan invariant #2: retire BEFORE removing from live-set so
            // a concurrent snapshot never drops the peer's counters.
            torrent_stats.retire_peer(&peer_stats);
            live_peer_stats
                .lock()
                .expect("live_peer_stats poisoned")
                .remove(&slot);
            release_peer_id(&active_peer_ids, peer_id);
            release_peer_slot(&global_peer_count, &torrent_peer_count);
            shaper.drop_peer(slot);
        });
        self.tasks.lock().await.push(task);
        Ok(())
    }

    /// Bind a `TcpListener` on `addr` and route inbound BitTorrent connections
    /// to the matching torrent by `info_hash` (A2).
    ///
    /// Each accepted connection:
    /// 1. Passes the peer address through the **first** torrent's
    ///    [`PeerFilter`] — see "SSRF note" below.
    /// 2. Reads the peer's handshake via [`read_handshake`] to learn
    ///    `info_hash`.
    /// 3. Looks up the target torrent; unknown `info_hash` ⇒ drop silently
    ///    (no bytes written).
    /// 4. Checks peer-ID collision against the target torrent's active set;
    ///    collision ⇒ drop silently (plan invariant #6).
    /// 5. Writes our handshake via [`write_handshake`] and spawns a
    ///    [`PeerConn`].
    ///
    /// **SSRF note**: because the target torrent isn't known until after we
    /// read the peer handshake, the per-torrent [`PeerFilter`] can't be
    /// consulted on the accept itself. We apply a global filter (defaulted to
    /// the inbound config's) as a pre-check. In practice the same
    /// [`DefaultPeerFilter`] policy should apply to every torrent — this is
    /// the recommended shape.
    ///
    /// **Reachability**: M2 inbound works only on LAN or with a manually
    /// forwarded port. `UPnP` / `NAT-PMP` lands in M5.
    ///
    /// Returns the resolved bound address (useful when `addr` uses port 0).
    ///
    /// # Errors
    ///
    /// Surfaces [`std::io::Error`] if the bind fails.
    pub async fn listen(
        self: &Arc<Self>,
        addr: SocketAddr,
        cfg: ListenConfig,
    ) -> Result<SocketAddr, std::io::Error> {
        let listener = TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        tracing::info!(%bound, "engine: inbound listener bound");
        // Record the bound port for the BEP 10 extension handshake `p`
        // field — every subsequent attach_stream stamps this into the
        // outgoing handshake so PEX can advertise our reachable address.
        self.listen_port.store(bound.port(), Ordering::Relaxed);
        let engine = Arc::clone(self);
        let token = self.shutdown_token.child_token();
        let task = tokio::spawn(async move {
            let mut handler_tasks: Vec<JoinHandle<()>> = Vec::new();
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        tracing::info!("engine: listener shutting down");
                        break;
                    }
                    result = listener.accept() => match result {
                        Ok((stream, peer_addr)) => {
                            let engine = Arc::clone(&engine);
                            let cfg = cfg.clone();
                            let h = tokio::spawn(async move {
                                engine.handle_inbound(stream, peer_addr, cfg).await;
                            });
                            handler_tasks.push(h);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    },
                }
            }
            let grace = Duration::from_secs(10);
            for h in handler_tasks {
                let _ = tokio::time::timeout(grace, h).await;
            }
        });
        self.tasks.lock().await.push(task);
        Ok(bound)
    }

    async fn handle_inbound(
        &self,
        mut stream: TcpStream,
        peer_addr: SocketAddr,
        cfg: ListenConfig,
    ) {
        // Pre-handshake peer filter: apply the inbound-global filter before we
        // spend any cycles on the handshake. This blocks SSRF attempts that
        // come from a configured inbound source (e.g. 127.0.0.1 listener with
        // strict filter).
        if !cfg.peer_filter.allow(peer_addr) {
            tracing::debug!(%peer_addr, "inbound peer filtered");
            return;
        }
        let remote = match read_handshake(&mut stream, cfg.handshake_timeout).await {
            Ok(hs) => hs,
            Err(e) => {
                tracing::debug!(%peer_addr, error = %e, "inbound handshake read failed");
                return;
            }
        };
        let Some(snapshot) = self.snapshot_by_info_hash(remote.info_hash).await else {
            tracing::debug!(%peer_addr, "inbound info_hash not registered; dropping");
            return;
        };
        // Cap check before the handshake reply (plan doc A2 §caps). Exceeding
        // either the global or per-torrent cap is a silent drop: we don't
        // want to advertise our peer-id when we know we'll immediately
        // disconnect.
        if let Err(scope) = reserve_peer_slot(
            &self.global_peer_count,
            self.global_peer_cap,
            &snapshot.peer_count,
            snapshot.peer_cap,
        ) {
            tracing::debug!(%peer_addr, ?scope, "inbound peer-cap exceeded; silent drop");
            return;
        }
        if !reserve_peer_id(&snapshot.active_peer_ids, remote.peer_id) {
            // Silent drop — do not reply (plan invariant #6: avoid leaking
            // "this peer-id is already connected").
            release_peer_slot(&self.global_peer_count, &snapshot.peer_count);
            tracing::debug!(%peer_addr, "inbound peer-id collision; silent drop");
            return;
        }
        if let Err(e) = write_handshake(&mut stream, &snapshot.cfg, cfg.handshake_timeout).await {
            tracing::debug!(%peer_addr, error = %e, "inbound handshake write failed");
            release_peer_id(&snapshot.active_peer_ids, remote.peer_id);
            release_peer_slot(&self.global_peer_count, &snapshot.peer_count);
            return;
        }
        // From here, the slot+peer-id reservations must be released by the
        // spawned peer task — spawn_peer_task takes ownership of both. The
        // exception is the `SessionClosed` path inside `spawn_peer_task`,
        // which releases them itself.
        let mut snapshot = snapshot;
        snapshot.cfg.remote_addr = Some(peer_addr);
        if let Err(e) = self.spawn_peer_task(snapshot, stream, remote).await {
            tracing::debug!(%peer_addr, error = %e, "inbound spawn failed");
        }
    }
}

/// Configuration for [`Engine::listen`].
#[derive(Clone)]
pub struct ListenConfig {
    /// Filter applied to the remote address of every accepted connection,
    /// before the handshake read. Typically the same [`DefaultPeerFilter`]
    /// used for outbound.
    pub peer_filter: Arc<dyn PeerFilter>,
    /// Budget for the combined handshake read + reply.
    pub handshake_timeout: Duration,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            peer_filter: Arc::new(DefaultPeerFilter::default()),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone)]
struct Snapshot {
    cfg: PeerConfig,
    peer_to_session_tx: mpsc::Sender<PeerToSession>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    peer_filter: Arc<dyn PeerFilter>,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    active_peer_ids: Arc<StdMutex<HashSet<[u8; 20]>>>,
    peer_count: Arc<AtomicUsize>,
    peer_cap: usize,
    /// Used by `spawn_peer_task` to register peer-tier shaper buckets
    /// against the correct torrent tier (#22 plan).
    torrent_id: TorrentId,
    /// ADR-0014 stats plumbing — `spawn_peer_task` registers a fresh
    /// `Arc<PeerStats>` here and retires it on exit.
    torrent_stats: Arc<crate::session::stats::PerTorrentStats>,
    live_peer_stats: Arc<StdMutex<HashMap<PeerSlot, Arc<crate::session::stats::PeerStats>>>>,
}
