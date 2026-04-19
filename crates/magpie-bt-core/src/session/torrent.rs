//! Per-torrent actor.
//!
//! Owns the picker, the in-progress piece buffers, the storage handle, and the
//! peer registry. All peer tasks talk to it over `mpsc`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use magpie_bt_wire::{BLOCK_SIZE, BlockRequest};
use tokio::sync::mpsc;

use crate::alerts::{Alert, AlertErrorCode, AlertQueue};
use crate::ids::TorrentId;
use crate::picker::Picker;
use crate::session::disk::{DiskCompletion, DiskError, DiskOp};
use crate::session::messages::{PeerSlot, PeerToSession, SessionCommand, SessionToPeer};
use crate::session::peer::DEFAULT_PER_PEER_IN_FLIGHT;
use crate::session::pex::PexState;
use crate::session::read_cache::ReadCache;

/// Static configuration of a single torrent that the session needs to run.
#[derive(Debug, Clone)]
pub struct TorrentParams {
    /// Total number of pieces.
    pub piece_count: u32,
    /// Standard piece length, in bytes (last piece may be shorter).
    pub piece_length: u64,
    /// Total file size, in bytes.
    pub total_length: u64,
    /// Concatenated SHA-1 piece hashes (`20 * piece_count` bytes).
    pub piece_hashes: Vec<u8>,
    /// BEP 27 private flag. When `true`, peer-discovery subsystems
    /// (DHT/PEX/LSD — all M3+) **must** suppress gossip of this torrent's
    /// peers. The flag is authoritative at the session layer; M3 DHT/PEX
    /// implementations check `is_private()` before announcing. M2 has no
    /// gossip surface, so honouring the flag is a forward-contract only.
    pub private: bool,
}

impl TorrentParams {
    /// BEP 27: torrent is marked private.
    #[must_use]
    pub const fn is_private(&self) -> bool {
        self.private
    }
}

/// Errors returned by [`TorrentParams::validate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TorrentParamsError {
    /// `piece_count` was zero — a torrent must have at least one piece.
    #[error("piece_count must be > 0")]
    ZeroPieceCount,
    /// `piece_length` was zero.
    #[error("piece_length must be > 0")]
    ZeroPieceLength,
    /// `total_length` was zero.
    #[error("total_length must be > 0")]
    ZeroTotalLength,
    /// `piece_hashes.len()` did not equal `20 * piece_count`.
    #[error("piece_hashes length {actual} does not match 20 * piece_count = {expected}")]
    PieceHashesLength {
        /// Expected length (`20 * piece_count`).
        expected: usize,
        /// Actual length we observed.
        actual: usize,
    },
    /// Final-piece geometry is impossible: `total_length` exceeds
    /// `piece_count * piece_length`.
    #[error(
        "total_length {total} exceeds piece_count * piece_length = {piece_count} * {piece_length}"
    )]
    TotalLengthOverflow {
        /// `total_length`.
        total: u64,
        /// `piece_count`.
        piece_count: u32,
        /// `piece_length`.
        piece_length: u64,
    },
}

impl TorrentParams {
    /// Validate invariants the session and disk-writer rely on (E14 hardening).
    /// The session and `DiskWriter` would otherwise panic on out-of-range
    /// piece-hash slices or under-sized buffers.
    ///
    /// # Errors
    ///
    /// See [`TorrentParamsError`] variants.
    pub fn validate(&self) -> Result<(), TorrentParamsError> {
        if self.piece_count == 0 {
            return Err(TorrentParamsError::ZeroPieceCount);
        }
        if self.piece_length == 0 {
            return Err(TorrentParamsError::ZeroPieceLength);
        }
        if self.total_length == 0 {
            return Err(TorrentParamsError::ZeroTotalLength);
        }
        let expected = (self.piece_count as usize) * 20;
        if self.piece_hashes.len() != expected {
            return Err(TorrentParamsError::PieceHashesLength {
                expected,
                actual: self.piece_hashes.len(),
            });
        }
        let max_total = u64::from(self.piece_count) * self.piece_length;
        if self.total_length > max_total {
            return Err(TorrentParamsError::TotalLengthOverflow {
                total: self.total_length,
                piece_count: self.piece_count,
                piece_length: self.piece_length,
            });
        }
        Ok(())
    }
}

impl TorrentParams {
    /// Length of the given piece in bytes (handles short final piece).
    #[must_use]
    pub fn piece_size(&self, piece: u32) -> u32 {
        if u64::from(piece + 1) * self.piece_length <= self.total_length {
            u32::try_from(self.piece_length).unwrap_or(u32::MAX)
        } else {
            let remainder = self.total_length - u64::from(piece) * self.piece_length;
            u32::try_from(remainder).unwrap_or(u32::MAX)
        }
    }

    /// Byte offset within the torrent for the start of the given piece.
    #[must_use]
    pub const fn piece_offset(&self, piece: u32) -> u64 {
        (piece as u64) * self.piece_length
    }

    fn piece_hash(&self, piece: u32) -> [u8; 20] {
        let start = (piece as usize) * 20;
        let mut out = [0u8; 20];
        out.copy_from_slice(&self.piece_hashes[start..start + 20]);
        out
    }
}

/// In-progress piece — accumulates blocks until full, then verifies + commits.
#[derive(Debug)]
struct InProgressPiece {
    buffer: Vec<u8>,
    /// Bitmap (one byte per block) of received blocks.
    received: Vec<bool>,
    /// Per-block claim: which peer (if any) is assigned this block.
    claimed: Vec<Option<PeerSlot>>,
    block_count: u32,
    received_count: u32,
}

impl InProgressPiece {
    fn new(piece_size: u32) -> Self {
        let block_count = piece_size.div_ceil(BLOCK_SIZE);
        Self {
            buffer: vec![0u8; piece_size as usize],
            received: vec![false; block_count as usize],
            claimed: vec![None; block_count as usize],
            block_count,
            received_count: 0,
        }
    }

    fn block_size_at(idx: u32, piece_size: u32) -> u32 {
        let start = idx * BLOCK_SIZE;
        (piece_size - start).min(BLOCK_SIZE)
    }
}

/// Per-peer bookkeeping inside the torrent actor.
#[allow(clippy::struct_excessive_bools)]
struct PeerState {
    tx: mpsc::UnboundedSender<SessionToPeer>,
    /// Bitfield of pieces this peer has (as `Vec<bool>` for simplicity).
    have: Vec<bool>,
    /// Peer is choking us → we cannot request from them.
    choking_us: bool,
    /// We told the peer we are interested.
    we_are_interested: bool,
    /// Number of outstanding requests we have on this peer.
    in_flight: u32,
    max_in_flight: u32,
    /// M2 seed-side: the peer has sent us Interested.
    peer_interested: bool,
    /// M2 seed-side: we have not sent Unchoke to this peer yet.
    we_choking: bool,
    /// Whether the peer supports BEP 6 Fast Extension (negotiated in
    /// handshake); controls whether we can send `RejectRequest`.
    supports_fast: bool,
    /// Remote socket address, if known. Used for outbound PEX.
    addr: Option<SocketAddr>,
    /// Whether the peer announced `HaveAll` during the initial exchange.
    /// Tracked so that after a metadata-transition (magnet-link path) we
    /// can re-expand the bitfield to the correct piece count.
    announced_have_all: bool,
}

impl PeerState {
    fn new(tx: mpsc::UnboundedSender<SessionToPeer>, piece_count: u32, max_in_flight: u32) -> Self {
        Self {
            tx,
            have: vec![false; piece_count as usize],
            choking_us: true,
            we_are_interested: false,
            in_flight: 0,
            max_in_flight,
            peer_interested: false,
            we_choking: true,
            supports_fast: false,
            addr: None,
            announced_have_all: false,
        }
    }

    const fn can_request(&self) -> bool {
        !self.choking_us && self.in_flight < self.max_in_flight
    }
}

/// State of a torrent download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    /// Still leeching.
    Downloading,
    /// All pieces verified and written to storage.
    Completed,
}

/// The per-torrent actor.
///
/// Disk verification + commit are *not* performed inline. The session holds
/// a [`DiskOp`] sender plus a [`DiskCompletion`] receiver; pieces are
/// finalised by sending `DiskOp::VerifyAndWrite` to the
/// [`DiskWriter`](super::disk::DiskWriter) task and updating state when the
/// matching completion arrives.
pub struct TorrentSession {
    torrent_id: TorrentId,
    params: TorrentParams,
    info_hash: [u8; 20],
    alerts: Arc<AlertQueue>,
    picker: Picker,
    in_progress: HashMap<u32, InProgressPiece>,
    peers: HashMap<PeerSlot, PeerState>,
    rx: mpsc::Receiver<PeerToSession>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    disk_tx: mpsc::Sender<DiskOp>,
    completion_tx: mpsc::UnboundedSender<DiskCompletion>,
    completion_rx: mpsc::UnboundedReceiver<DiskCompletion>,
    state: TorrentState,
    /// Session-global read cache for seed-side block serving (ADR-0018).
    /// Arc'd so spawned reader tasks can consult it without holding the
    /// actor loop.
    read_cache: Arc<ReadCache>,
    /// ADR-0019: fires exactly once on the leech→seed transition. Covers
    /// the whole 5-step block (`Alert::TorrentComplete` → choker swap →
    /// re-eval → `NotInterested` broadcast → tracker `event=completed`). A
    /// single flag for the whole block avoids the per-step race where a
    /// second completion arrives mid-transition and fires a duplicate
    /// alert. Torrents loaded via `initial_have` start in
    /// `TorrentState::Completed` — the flag is pre-set in that path so no
    /// transition fires on resume.
    completion_fired: bool,
    /// G1: when true, the actor stops scheduling new requests and refuses
    /// to serve incoming block requests. Connected peers are choked but kept
    /// alive so resume is cheap. Toggled by [`SessionCommand::Pause`] and
    /// [`SessionCommand::Resume`].
    paused: bool,
    /// BEP 9 metadata assembler for magnet-link torrents. `Some` while we
    /// are still fetching the info dict; `None` for normal torrents.
    metadata_assembler: Option<crate::session::metadata_exchange::MetadataAssembler>,
    /// Raw info dict bytes, stored after successful metadata assembly or
    /// when the torrent was added via `add_torrent()`. Used to serve
    /// `ut_metadata` Data requests to peers.
    info_dict_bytes: Option<Vec<u8>>,
    /// Number of consecutive metadata verification failures (hash mismatch
    /// or parse error). After exceeding the limit we stop retrying to avoid
    /// infinite loops from persistently malicious peers.
    metadata_verify_failures: u32,
    /// BEP 11 Peer Exchange state.
    pex: PexState,
    /// Addresses discovered via inbound PEX messages, buffered until the
    /// engine drains them via [`TorrentSession::drain_pex_discovered`].
    pex_discovered: Vec<SocketAddr>,
    /// Outbound PEX round interval. Defaults to
    /// [`crate::session::pex::PEX_INTERVAL`] (60 s); overridable via
    /// [`TorrentSession::set_pex_interval`] for tests that need a faster
    /// round than real-world peer exchange.
    pex_interval: std::time::Duration,
    /// Per-torrent connected-peer cap. Used to limit how many PEX-discovered
    /// addresses we buffer (no point discovering peers we cannot connect to).
    peer_cap: usize,
}

/// Default capacity for the [`SessionCommand`] channel returned by
/// [`TorrentSession::new`]. Commands are low-rate (one per peer connect plus
/// the occasional shutdown), so a small bound suffices.
pub const SESSION_COMMAND_CAPACITY: usize = 16;

impl TorrentSession {
    /// Construct a fresh session bound to the given disk-writer submission
    /// channel. Returns the session plus the [`SessionCommand`] sender the
    /// caller (typically [`Engine`](crate::engine::Engine)) uses to register
    /// peers and trigger shutdown.
    ///
    /// `info_hash` is used to key the read cache; `read_cache` should be the
    /// session-global cache instance shared across all torrents.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        torrent_id: TorrentId,
        params: TorrentParams,
        info_hash: [u8; 20],
        alerts: Arc<AlertQueue>,
        rx: mpsc::Receiver<PeerToSession>,
        disk_tx: mpsc::Sender<DiskOp>,
        read_cache: Arc<ReadCache>,
        peer_cap: usize,
    ) -> (Self, mpsc::Sender<SessionCommand>) {
        // Unbounded (D1): outstanding completions are naturally capped by the
        // disk-op queue. Bounding both legs deadlocks the actor↔writer pair.
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(SESSION_COMMAND_CAPACITY);
        let picker = Picker::new(params.piece_count);
        let pex = PexState::new(params.private);
        let session = Self {
            torrent_id,
            params,
            info_hash,
            read_cache,
            completion_fired: false,
            alerts,
            picker,
            in_progress: HashMap::new(),
            peers: HashMap::new(),
            rx,
            cmd_rx,
            disk_tx,
            completion_tx,
            completion_rx,
            state: TorrentState::Downloading,
            paused: false,
            metadata_assembler: None,
            info_dict_bytes: None,
            metadata_verify_failures: 0,
            pex,
            pex_discovered: Vec::new(),
            pex_interval: crate::session::pex::PEX_INTERVAL,
            peer_cap,
        };
        (session, cmd_tx)
    }

    /// Attach a metadata assembler, turning this session into a
    /// "metadata-fetching" session (magnet-link path).
    pub fn set_metadata_assembler(
        &mut self,
        assembler: crate::session::metadata_exchange::MetadataAssembler,
    ) {
        self.metadata_assembler = Some(assembler);
    }

    /// Store the raw info dict bytes so we can serve `ut_metadata` requests.
    pub fn set_info_dict_bytes(&mut self, bytes: Vec<u8>) {
        self.info_dict_bytes = Some(bytes);
    }

    /// Register a new peer with an explicit `max_in_flight` ceiling. Returns
    /// `false` if `slot` is already known.
    ///
    /// Also pushes our initial advertisement onto the peer's tx so the
    /// remote learns what we have. Sending here (rather than from
    /// `handle_connected`) avoids a race where `Connected` arrives on
    /// `peer_to_session_rx` before the actor has processed `RegisterPeer`.
    ///
    /// **#21 optimisation**: when `supports_fast = true` (both sides
    /// negotiated BEP 6), use `HaveAll` / `HaveNone` (1-byte frames)
    /// when our current bitmap is fully-set or fully-unset. Otherwise
    /// send a full `Bitfield` (BEP 3, universal).
    pub fn register_peer_with(
        &mut self,
        slot: PeerSlot,
        tx: mpsc::UnboundedSender<SessionToPeer>,
        max_in_flight: u32,
        supports_fast: bool,
    ) -> bool {
        if self.peers.contains_key(&slot) {
            return false;
        }
        let total = self.params.piece_count;
        let missing = self.picker.missing_count();
        let advert = if supports_fast && missing == 0 {
            SessionToPeer::SendHaveAll
        } else if supports_fast && missing == total {
            SessionToPeer::SendHaveNone
        } else {
            SessionToPeer::SendBitfield(pack_bitfield(&self.picker, total))
        };
        let _ = tx.send(advert);
        self.peers.insert(
            slot,
            PeerState::new(tx, self.params.piece_count, max_in_flight),
        );
        true
    }

    /// Convenience: [`register_peer_with`](Self::register_peer_with) using
    /// [`DEFAULT_PER_PEER_IN_FLIGHT`] and `supports_fast = false` (always
    /// sends a `Bitfield`). Test-only — production callers should route
    /// through `SessionCommand::RegisterPeer` so fast-ext is preserved.
    pub fn register_peer(
        &mut self,
        slot: PeerSlot,
        tx: mpsc::UnboundedSender<SessionToPeer>,
    ) -> bool {
        self.register_peer_with(slot, tx, DEFAULT_PER_PEER_IN_FLIGHT, false)
    }

    /// Drain buffered PEX-discovered peer addresses. The engine calls this
    /// periodically and feeds the returned addresses into `add_peer`.
    pub fn drain_pex_discovered(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.pex_discovered)
    }

    /// Override the outbound PEX round interval (default 60 s). Test-only
    /// knob exposed to keep the M3 PEX-discovery integration test bounded.
    pub const fn set_pex_interval(&mut self, interval: std::time::Duration) {
        self.pex_interval = interval;
    }

    /// Mark the listed pieces as already-verified. Used by resume-from-disk
    /// and seed-mode tests so the actor can answer `BlockRequested`
    /// immediately without first leeching the piece.
    ///
    /// `have` must be exactly `piece_count` elements long. Shorter or longer
    /// vectors are silently ignored.
    pub fn apply_initial_have(&mut self, have: &[bool]) {
        if have.len() != self.params.piece_count as usize {
            return;
        }
        for (i, h) in have.iter().enumerate() {
            if *h {
                self.picker.mark_have(u32::try_from(i).unwrap_or(u32::MAX));
            }
        }
        // If we start fully complete, transition straight to Completed so
        // leech-side logic doesn't attempt to pick. Pre-set
        // `completion_fired` per ADR-0019 — resume-from-complete does not
        // emit the transition alert/broadcast (nobody transitioned, the
        // torrent was already whole when loaded).
        if self.picker.missing_count() == 0 {
            self.state = TorrentState::Completed;
            self.completion_fired = true;
        }
    }

    /// Drive the actor until completion or every input channel closes.
    pub async fn run(mut self) -> TorrentState {
        let span = tracing::info_span!(
            "torrent",
            piece_count = self.params.piece_count,
            total_length = self.params.total_length,
        );
        let _enter = span.enter();
        tracing::debug!("torrent session started");
        let result = self.run_inner().await;
        tracing::info!(state = ?result, "torrent session finished");
        result
    }

    async fn run_inner(&mut self) -> TorrentState {
        // M2: stay in the loop while Downloading OR Completed — a completed
        // torrent still needs to serve inbound block requests (seed mode).
        // Exit only on explicit Shutdown or channel close.
        let mut pex_interval = tokio::time::interval(self.pex_interval);
        // Don't fire immediately on start — wait for the first interval.
        pex_interval.reset();
        loop {
            // Biased: cmd_rx first so RegisterPeer is always processed
            // before any peer messages on rx (spawn_peer awaits
            // cmd_tx.send() before tokio::spawn, guaranteeing ordering).
            // completion_rx second to clear disk backpressure promptly.
            tokio::select! {
                biased;
                cmd = self.cmd_rx.recv() => match cmd {
                    None | Some(SessionCommand::Shutdown) => break,
                    Some(SessionCommand::RegisterPeer { slot, tx, max_in_flight, supports_fast }) => {
                        self.register_peer_with(slot, tx, max_in_flight, supports_fast);
                    }
                    Some(SessionCommand::Pause) => self.set_paused(true),
                    Some(SessionCommand::Resume) => self.set_paused(false),
                    Some(SessionCommand::DrainPexDiscovered { reply }) => {
                        let drained = self.drain_pex_discovered();
                        let _ = reply.send(drained);
                    }
                    Some(SessionCommand::BitfieldSnapshot { reply }) => {
                        let _ = reply.send(self.picker.have_snapshot());
                    }
                },
                completion = self.completion_rx.recv() => match completion {
                    None => break,
                    Some(c) => self.handle_completion(c),
                },
                msg = self.rx.recv() => match msg {
                    None => break,
                    Some(m) => self.handle(m).await,
                },
                _ = pex_interval.tick() => {
                    self.send_pex_round();
                },
            }
            // Only the leech path schedules requests outbound, and never
            // while paused (G1).
            if self.state == TorrentState::Downloading && !self.paused {
                self.schedule();
            }
        }
        for peer in self.peers.values() {
            let _ = peer.tx.send(SessionToPeer::Shutdown);
        }
        self.state
    }

    fn handle_completion(&mut self, completion: DiskCompletion) {
        // Drop the "awaiting verification" marker now that disk has weighed in.
        self.in_progress.remove(&completion.piece);
        match completion.result {
            Ok(()) => {
                tracing::debug!(piece = completion.piece, "piece verified and committed");
                self.picker.mark_have(completion.piece);
                self.alerts.push(Alert::PieceCompleted {
                    torrent: self.torrent_id,
                    piece: completion.piece,
                });
                for p in self.peers.values() {
                    let _ = p.tx.send(SessionToPeer::Have(completion.piece));
                }
                if self.picker.missing_count() == 0 {
                    self.state = TorrentState::Completed;
                    self.maybe_fire_completion_transition();
                }
            }
            Err(DiskError::HashMismatch) => {
                tracing::warn!(
                    piece = completion.piece,
                    "piece hash mismatch — re-requesting"
                );
                self.alerts.push(Alert::Error {
                    torrent: self.torrent_id,
                    code: AlertErrorCode::HashMismatch,
                });
                // Piece marker is gone and picker still sees it as missing,
                // so next schedule pass will re-claim it from scratch.
            }
            Err(DiskError::Io) => {
                self.alerts.push(Alert::Error {
                    torrent: self.torrent_id,
                    code: AlertErrorCode::StorageIo,
                });
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, msg: PeerToSession) {
        match msg {
            PeerToSession::Connected {
                slot,
                supports_fast,
                addr,
                ..
            } => {
                tracing::debug!(slot = slot.0, supports_fast, "peer connected");
                if let Some(p) = self.peers.get_mut(&slot) {
                    p.supports_fast = supports_fast;
                    p.addr = addr;
                }
                // Note: the initial Bitfield advert is sent by
                // `register_peer_with` to dodge a Connected-before-Register
                // race. HaveAll/HaveNone fast-ext optimization is a follow-up.
                self.alerts.push(Alert::PeerConnected { torrent: self.torrent_id, peer: slot });
            }
            PeerToSession::Disconnected { slot, ref reason } => {
                tracing::debug!(slot = slot.0, ?reason, "peer disconnected");
                if let Some(peer) = self.peers.remove(&slot) {
                    // Forget the peer's bitfield contribution.
                    self.picker.forget_peer_bitfield(&peer.have);
                    // Release any blocks claimed by this peer.
                    self.release_claims(slot);
                }
                self.pex.peer_disconnected(slot);
                self.alerts.push(Alert::PeerDisconnected { torrent: self.torrent_id, peer: slot });
            }
            PeerToSession::Choked { slot } => {
                if let Some(p) = self.peers.get_mut(&slot) {
                    p.choking_us = true;
                    p.in_flight = 0;
                }
                self.release_claims(slot);
            }
            PeerToSession::Unchoked { slot } => {
                if let Some(p) = self.peers.get_mut(&slot) {
                    p.choking_us = false;
                }
            }
            PeerToSession::Have { slot, piece } => {
                // S6: out-of-range piece is a protocol violation — drop peer.
                if piece >= self.params.piece_count {
                    self.kick_peer(slot, AlertErrorCode::PeerProtocol);
                    return;
                }
                if let Some(p) = self.peers.get_mut(&slot)
                    && !p.have[piece as usize]
                {
                    p.have[piece as usize] = true;
                    self.picker
                        .observe_peer_bitfield(&single_bit(piece, p.have.len()));
                }
                self.maybe_express_interest(slot);
            }
            PeerToSession::Bitfield { slot, bytes } => {
                // S4 + S5: validate length and spare-bit invariant before
                // touching the picker. A peer that lies here is hostile or
                // buggy — disconnect.
                match decode_bitfield_strict(&bytes, self.params.piece_count) {
                    Ok(bits) => {
                        if let Some(p) = self.peers.get_mut(&slot) {
                            self.picker.forget_peer_bitfield(&p.have);
                            p.have = bits;
                            self.picker.observe_peer_bitfield(&p.have);
                        } else {
                            tracing::warn!(slot = slot.0, "Bitfield for unknown peer — biased select invariant violated?");
                        }
                        self.maybe_express_interest(slot);
                    }
                    Err(_) => {
                        self.kick_peer(slot, AlertErrorCode::PeerProtocol);
                    }
                }
            }
            PeerToSession::HaveAll { slot } => {
                if let Some(p) = self.peers.get_mut(&slot) {
                    self.picker.forget_peer_bitfield(&p.have);
                    p.have = vec![true; self.params.piece_count as usize];
                    p.announced_have_all = true;
                    self.picker.observe_peer_bitfield(&p.have);
                } else {
                    tracing::warn!(slot = slot.0, "HaveAll for unknown peer — biased select invariant violated?");
                }
                self.maybe_express_interest(slot);
            }
            PeerToSession::HaveNone { slot } => {
                if let Some(p) = self.peers.get_mut(&slot) {
                    self.picker.forget_peer_bitfield(&p.have);
                    p.have = vec![false; self.params.piece_count as usize];
                } else {
                    tracing::warn!(slot = slot.0, "HaveNone for unknown peer — biased select invariant violated?");
                }
                self.maybe_express_interest(slot);
            }
            PeerToSession::Rejected { slot, req } => {
                self.release_block_claim(slot, req);
            }
            PeerToSession::BlockReceived {
                slot,
                piece,
                offset,
                data,
            } => {
                self.handle_block(slot, piece, offset, &data).await;
            }
            PeerToSession::Interested { slot } => {
                self.handle_peer_interest(slot, true);
            }
            PeerToSession::NotInterested { slot } => {
                self.handle_peer_interest(slot, false);
            }
            PeerToSession::BlockRequested { slot, req } => {
                self.handle_block_request(slot, req);
            }
            PeerToSession::RequestCancelled { slot, req } => {
                tracing::debug!(
                    slot = slot.0,
                    piece = req.piece,
                    offset = req.offset,
                    "peer cancelled request (tracked pre-PeerUploadQueue wiring)"
                );
            }
            PeerToSession::ExtensionHandshake {
                slot,
                ref extensions,
                metadata_size,
                ref client,
                listen_port,
            } => {
                tracing::debug!(
                    slot = slot.0,
                    ?extensions,
                    ?metadata_size,
                    ?client,
                    ?listen_port,
                    "peer extension handshake"
                );
                self.handle_extension_handshake(slot, extensions, metadata_size, listen_port);
            }
            PeerToSession::ExtensionMessage {
                slot,
                ref extension_name,
                ref payload,
            } => {
                self.handle_extension_message(slot, extension_name, payload);
            }
        }
    }

    /// ADR-0019 completion transition. Fires **exactly once** per torrent
    /// lifecycle, guarded by `completion_fired` across the whole 5-step
    /// sequence (not per step — avoids the race where a second completion
    /// mid-transition duplicates work):
    ///
    /// 1. `Alert::TorrentComplete` — signal to consumer.
    /// 2. Choker swap `Leech→Seed` + reset optimistic timer. Deferred to
    ///    choker wiring; currently a no-op hook.
    /// 3. Immediate seed-unchoke re-eval. Deferred (same as step 2).
    /// 4. `NotInterested` broadcast — **after** the unchoke round per
    ///    plan invariant #11 (reversing this order makes remote peers
    ///    drop their `Interested` toward us before seeing our unchoke
    ///    decision, leaving slots idle).
    /// 5. Fire-and-forget tracker `event=completed`. Deferred — tracker
    ///    handle lives on the Engine side, not inside the actor.
    fn maybe_fire_completion_transition(&mut self) {
        if self.completion_fired {
            return;
        }
        self.completion_fired = true;
        // Step 1: emit the transition alert.
        self.alerts.push(Alert::TorrentComplete { torrent: self.torrent_id });
        // Steps 2 + 3: choker swap and immediate re-eval. Choker isn't
        // wired into the actor yet; the hook is documented for the
        // follow-up wiring step.
        // Step 4: broadcast NotInterested to every peer. This must follow
        // the unchoke round (steps 2-3) so the peer sees our unchoke
        // decision first; ordering is a load-bearing plan invariant.
        for peer in self.peers.values_mut() {
            if peer.we_are_interested {
                peer.we_are_interested = false;
                let _ = peer.tx.send(SessionToPeer::SetInterested(false));
            }
        }
        // Step 5: tracker event=completed. Hooked here when the tracker
        // handle lands in session state (Stage 3).
    }

    /// G1: toggle paused state. Idempotent; only broadcasts choke/unchoke
    /// on a real transition. On pause: send `Choke` to every connected peer
    /// (and flip `we_choking` so the existing [`handle_block_request`] gate
    /// also drops new requests). On resume: send `Unchoke` to every peer
    /// that's currently interested in us (M2 pre-choker baseline matches
    /// [`handle_peer_interest`]).
    fn set_paused(&mut self, want_paused: bool) {
        if self.paused == want_paused {
            return;
        }
        self.paused = want_paused;
        if want_paused {
            // Step A: broadcast Choke so the remote stops sending new
            // block requests we'd otherwise serve.
            for peer in self.peers.values_mut() {
                if !peer.we_choking {
                    peer.we_choking = true;
                    let _ = peer.tx.send(SessionToPeer::Choke);
                }
            }
            // Step B (#20): cancel all outstanding outbound Requests. The
            // remote may already be mid-send for some, but canceling
            // releases our claim so the next resume+schedule pass can
            // re-request fresh. Walk `in_progress` to find every claimed
            // (slot, piece, block), send Cancel, release the claim.
            let piece_length_u32 = u32::try_from(self.params.piece_length).unwrap_or(u32::MAX);
            let piece_count = self.params.piece_count;
            let mut cancellations: Vec<(PeerSlot, BlockRequest)> = Vec::new();
            for (piece, in_prog) in &self.in_progress {
                let piece_size = if *piece + 1 == piece_count {
                    // Last piece may be short.
                    let tail =
                        self.params.total_length - u64::from(*piece) * self.params.piece_length;
                    u32::try_from(tail).unwrap_or(u32::MAX)
                } else {
                    piece_length_u32
                };
                for (idx, slot_opt) in in_prog.claimed.iter().enumerate() {
                    let Some(slot) = *slot_opt else { continue };
                    let idx_u32 = u32::try_from(idx).unwrap_or(u32::MAX);
                    let block_size = InProgressPiece::block_size_at(idx_u32, piece_size);
                    let offset = idx_u32 * BLOCK_SIZE;
                    cancellations.push((slot, BlockRequest::new(*piece, offset, block_size)));
                }
            }
            for (slot, req) in cancellations {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    let _ = peer.tx.send(SessionToPeer::Cancel(req));
                }
                self.release_block_claim(slot, req);
            }
        } else {
            for peer in self.peers.values_mut() {
                if peer.peer_interested && peer.we_choking {
                    peer.we_choking = false;
                    let _ = peer.tx.send(SessionToPeer::Unchoke);
                }
            }
        }
    }

    /// Seed-side: peer changed interest state. When a peer becomes
    /// interested we always unchoke in M2 pre-choker baseline — the full
    /// [`Unchoker`](super::Unchoker)-based choking lands with ADR-0012 in a
    /// follow-up step.
    fn handle_peer_interest(&mut self, slot: PeerSlot, interested: bool) {
        let paused = self.paused;
        let Some(peer) = self.peers.get_mut(&slot) else {
            return;
        };
        peer.peer_interested = interested;
        // G1: while paused, do not auto-unchoke — peer stays choked until
        // resume runs the broadcast loop.
        if interested && peer.we_choking && !paused {
            peer.we_choking = false;
            let _ = peer.tx.send(SessionToPeer::Unchoke);
        }
    }

    /// Seed-side: peer asked us for a block. Validates the request, checks
    /// we have the piece, and spawns a reader task that fetches via the
    /// read cache and sends back [`SessionToPeer::BlockReady`].
    #[allow(clippy::cast_possible_truncation)]
    fn handle_block_request(&self, slot: PeerSlot, req: BlockRequest) {
        // Protocol validation. An out-of-range piece or oversized block is
        // hostile/buggy; kick the peer.
        if req.piece >= self.params.piece_count || req.length > BLOCK_SIZE {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }
        let Some(peer) = self.peers.get(&slot) else {
            return;
        };
        // Choked peers do not get served (M2 pre-PeerUploadQueue baseline).
        if peer.we_choking {
            if peer.supports_fast {
                let _ = peer.tx.send(SessionToPeer::RejectRequest(req));
            }
            return;
        }
        // Reject-if-not-available: we only serve pieces we've fully
        // verified.
        if !self.picker.has_piece(req.piece) {
            if peer.supports_fast {
                let _ = peer.tx.send(SessionToPeer::RejectRequest(req));
            }
            return;
        }
        // Bounds-check the block offset against the piece length. A short
        // final piece needs special handling; the request's offset + length
        // must fit inside piece_size.
        let piece_size = self.params.piece_size(req.piece);
        if req.offset.saturating_add(req.length) > piece_size {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }

        let cache = Arc::clone(&self.read_cache);
        let disk_tx = self.disk_tx.clone();
        let tx = peer.tx.clone();
        let supports_fast = peer.supports_fast;
        let info_hash = self.info_hash;
        let piece_offset = self.params.piece_offset(req.piece);
        tokio::spawn(async move {
            let result = cache
                .get_or_load((info_hash, req.piece), piece_offset, piece_size, &disk_tx)
                .await;
            match result {
                Ok(piece_bytes) => {
                    let block =
                        piece_bytes.slice(req.offset as usize..(req.offset + req.length) as usize);
                    let _ = tx.send(SessionToPeer::BlockReady { req, data: block });
                }
                Err(_) => {
                    if supports_fast {
                        let _ = tx.send(SessionToPeer::RejectRequest(req));
                    }
                }
            }
        });
    }

    // --- BEP 9 / BEP 10 extension protocol handlers --------------------------

    fn handle_extension_handshake(
        &mut self,
        slot: PeerSlot,
        extensions: &HashMap<String, u8>,
        metadata_size: Option<u64>,
        listen_port: Option<u16>,
    ) {
        // If we have a metadata assembler (magnet-link path), learn the
        // metadata_size and start requesting pieces from this peer.
        if let Some(size) = metadata_size
            && let Some(assembler) = &mut self.metadata_assembler
        {
            assembler.set_total_size(size);
        }
        // BEP 11 reachability: rewrite the peer's address to (remote_ip,
        // their_listen_port) when the BEP 10 `p` field is present. This is
        // what PEX rounds advertise — without it, inbound peers' source
        // ports (ephemeral) leak into PEX and other peers can't dial back.
        if let Some(port) = listen_port
            && let Some(peer) = self.peers.get_mut(&slot)
            && let Some(current) = peer.addr
        {
            peer.addr = Some(SocketAddr::new(current.ip(), port));
        }
        // Check if the peer advertised ut_metadata and remember the id
        // so we can route extension messages and send requests.
        let _ut_metadata_supported = extensions.contains_key("ut_metadata");
        self.request_metadata_from_peer(slot);
    }

    fn handle_extension_message(
        &mut self,
        slot: PeerSlot,
        extension_name: &str,
        payload: &[u8],
    ) {
        match extension_name {
            "ut_metadata" => {
                self.handle_ut_metadata(slot, payload);
            }
            "ut_pex" => {
                self.handle_pex_inbound(slot, payload);
            }
            other => {
                tracing::debug!(
                    slot = slot.0,
                    extension = other,
                    "unknown extension message — ignoring"
                );
            }
        }
    }

    fn handle_ut_metadata(&mut self, slot: PeerSlot, payload: &[u8]) {
        use magpie_bt_wire::MetadataMessage;

        let msg = match MetadataMessage::decode(payload) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(slot = slot.0, error = %e, "bad ut_metadata message");
                return;
            }
        };

        match msg {
            MetadataMessage::Data {
                piece,
                total_size,
                data,
            } => {
                // Set total_size if the assembler hasn't seen it yet
                if let Some(assembler) = &mut self.metadata_assembler {
                    assembler.set_total_size(total_size);
                    if let Err(e) = assembler.receive_piece(piece, data) {
                        tracing::debug!(piece, error = %e, "metadata piece receive failed");
                        return;
                    }
                    if assembler.is_complete() {
                        self.try_complete_metadata();
                    }
                }
            }
            MetadataMessage::Reject { piece } => {
                if let Some(assembler) = &mut self.metadata_assembler {
                    assembler.receive_reject(piece);
                }
                // Try requesting from another peer
                self.request_metadata_from_any_peer();
            }
            MetadataMessage::Request { piece } => {
                // Serve metadata if we have it
                self.serve_metadata_piece(slot, piece);
            }
        }
    }

    /// Send `ut_metadata` request messages to a peer for pieces we still need.
    fn request_metadata_from_peer(&mut self, slot: PeerSlot) {
        let Some(assembler) = &mut self.metadata_assembler else {
            return;
        };
        if !assembler.has_size() {
            return;
        }
        // Request up to a few pieces from this peer
        let mut requested = 0;
        while requested < 4 {
            let Some(piece) = assembler.next_needed_piece() else {
                break;
            };
            assembler.mark_pending(piece, slot);
            let msg = magpie_bt_wire::MetadataMessage::Request { piece };
            let payload = msg.encode();
            if let Some(peer) = self.peers.get(&slot) {
                let _ = peer.tx.send(SessionToPeer::SendExtended {
                    extension_name: "ut_metadata".to_owned(),
                    payload: payload.into(),
                });
            }
            requested += 1;
        }
    }

    /// Try requesting metadata pieces from any peer that supports `ut_metadata`.
    fn request_metadata_from_any_peer(&mut self) {
        let slots: Vec<PeerSlot> = self.peers.keys().copied().collect();
        for slot in slots {
            if self.metadata_assembler.as_ref().is_none_or(|a| a.next_needed_piece().is_none()) {
                break;
            }
            self.request_metadata_from_peer(slot);
        }
    }

    /// Attempt to assemble and verify the metadata, then transition to
    /// a normal downloading session.
    fn try_complete_metadata(&mut self) {
        let Some(assembler) = &self.metadata_assembler else {
            return;
        };
        match assembler.verify_and_parse() {
            Ok((info_bytes, params)) => {
                tracing::info!("metadata assembly complete — transitioning to download");
                self.info_dict_bytes = Some(info_bytes);
                // Store the new params
                self.params = params;
                // Re-init the picker for the real piece count
                self.picker = Picker::new(self.params.piece_count);
                // Clear the assembler
                self.metadata_assembler = None;
                // Emit alert
                self.alerts.push(Alert::MetadataReceived {
                    torrent: self.torrent_id,
                });
                // Now we can start requesting real pieces from peers.
                // Update peer bitfield vectors to match new piece_count.
                // Peers that announced HaveAll get all-true; others start
                // at all-false and will re-announce via Have messages.
                let pc = self.params.piece_count as usize;
                for peer in self.peers.values_mut() {
                    if peer.announced_have_all {
                        peer.have = vec![true; pc];
                        self.picker.observe_peer_bitfield(&peer.have);
                    } else {
                        peer.have = vec![false; pc];
                    }
                }
                // Express interest in peers that have pieces we need.
                let slots: Vec<PeerSlot> = self.peers.keys().copied().collect();
                for slot in slots {
                    self.maybe_express_interest(slot);
                }
            }
            Err(e) => {
                self.metadata_verify_failures += 1;
                if self.metadata_verify_failures > 3 {
                    tracing::error!(
                        failures = self.metadata_verify_failures,
                        error = %e,
                        "metadata verification failed too many times — giving up"
                    );
                    self.metadata_assembler = None;
                    self.alerts.push(Alert::Error {
                        torrent: self.torrent_id,
                        code: AlertErrorCode::MetadataVerifyExhausted,
                    });
                    return;
                }
                tracing::warn!(
                    attempt = self.metadata_verify_failures,
                    error = %e,
                    "metadata verification failed — retrying"
                );
                // Clear assembler state and retry
                if let Some(assembler) = &mut self.metadata_assembler {
                    let info_hash = self.info_hash;
                    let total_size = assembler.total_size();
                    *assembler = crate::session::metadata_exchange::MetadataAssembler::new(info_hash);
                    if let Some(size) = total_size {
                        assembler.set_total_size(size);
                    }
                }
                self.request_metadata_from_any_peer();
            }
        }
    }

    /// Serve a metadata piece to a peer that requested it.
    #[allow(clippy::cast_possible_truncation)]
    fn serve_metadata_piece(&self, slot: PeerSlot, piece: u32) {
        let Some(info_bytes) = &self.info_dict_bytes else {
            // We don't have metadata to serve; send Reject.
            self.send_metadata_reject(slot, piece);
            return;
        };
        // Reject out-of-bounds piece indices before doing any arithmetic.
        let max_pieces = magpie_bt_wire::metadata_piece_count(info_bytes.len() as u64);
        if piece >= max_pieces {
            self.send_metadata_reject(slot, piece);
            return;
        }
        let total_size = info_bytes.len() as u64;
        let start = piece as usize * magpie_bt_wire::METADATA_PIECE_SIZE;
        if start >= info_bytes.len() {
            // Out of range — send Reject
            self.send_metadata_reject(slot, piece);
            return;
        }
        let end = (start + magpie_bt_wire::METADATA_PIECE_SIZE).min(info_bytes.len());
        let data = Bytes::copy_from_slice(&info_bytes[start..end]);
        let msg = magpie_bt_wire::MetadataMessage::Data {
            piece,
            total_size,
            data,
        };
        if let Some(peer) = self.peers.get(&slot) {
            let _ = peer.tx.send(SessionToPeer::SendExtended {
                extension_name: "ut_metadata".to_owned(),
                payload: msg.encode().into(),
            });
        }
    }

    fn send_metadata_reject(&self, slot: PeerSlot, piece: u32) {
        if let Some(peer) = self.peers.get(&slot) {
            let msg = magpie_bt_wire::MetadataMessage::Reject { piece };
            let _ = peer.tx.send(SessionToPeer::SendExtended {
                extension_name: "ut_metadata".to_owned(),
                payload: msg.encode().into(),
            });
        }
    }

    // ---- BEP 11 PEX ----

    /// Handle an inbound PEX message: decode, rate-limit, cap-check, and
    /// buffer discovered peers for the engine to drain via
    /// [`drain_pex_discovered`](Self::drain_pex_discovered).
    fn handle_pex_inbound(&mut self, slot: PeerSlot, payload: &[u8]) {
        if !self.pex.is_enabled() {
            tracing::debug!(slot = slot.0, "PEX message ignored (private torrent)");
            return;
        }

        // Inbound rate limiting: drop messages arriving faster than the
        // minimum interval from the same peer.
        let now = tokio::time::Instant::now();
        if !self.pex.should_accept_from(slot, now) {
            tracing::debug!(slot = slot.0, "PEX message rate-limited (too frequent)");
            return;
        }

        // Peer cap: skip discovery entirely when we're already at capacity.
        let current_peers = self.peers.len();
        if current_peers >= self.peer_cap {
            tracing::debug!(
                slot = slot.0,
                current_peers,
                peer_cap = self.peer_cap,
                "PEX message ignored (at peer cap)"
            );
            return;
        }

        match magpie_bt_wire::pex::PexMessage::decode(payload) {
            Ok(msg) => {
                self.pex.record_received(slot, now);
                let remaining = self.peer_cap.saturating_sub(current_peers);
                let addrs: Vec<SocketAddr> = msg
                    .added
                    .into_iter()
                    .map(|p| p.addr)
                    .take(remaining)
                    .collect();
                if !addrs.is_empty() {
                    tracing::debug!(
                        slot = slot.0,
                        count = addrs.len(),
                        "PEX: discovered peers"
                    );
                    self.pex_discovered.extend(addrs);
                }
                // Dropped peers are informational — we manage our own
                // connections independently.
            }
            Err(e) => {
                tracing::debug!(slot = slot.0, error = %e, "PEX: decode failed");
            }
        }
    }

    /// Periodic outbound PEX round: build the diff and send to all peers
    /// that are due for a PEX message.
    fn send_pex_round(&mut self) {
        if !self.pex.is_enabled() {
            return;
        }
        // Collect addresses of connected peers for the diff.
        let peer_addrs: HashMap<PeerSlot, SocketAddr> = self
            .peers
            .iter()
            .filter_map(|(slot, ps)| ps.addr.map(|a| (*slot, a)))
            .collect();

        let now = tokio::time::Instant::now();
        let Some(msg) = self.pex.build_message(&peer_addrs) else {
            return;
        };
        let encoded_payload: Bytes = Bytes::from(msg.encode());

        let slots: Vec<PeerSlot> = self.peers.keys().copied().collect();
        for slot in slots {
            if !self.pex.should_send_to(slot, now) {
                continue;
            }
            if let Some(peer) = self.peers.get(&slot) {
                let _ = peer.tx.send(SessionToPeer::SendExtended {
                    extension_name: "ut_pex".to_owned(),
                    payload: encoded_payload.clone(),
                });
                self.pex.record_sent(slot, now);
            }
        }
    }

    /// Send Shutdown to the peer and emit a typed error alert (S4/S5/S6/S7).
    /// The peer task will exit and we'll receive `Disconnected` shortly,
    /// which performs the actual cleanup.
    fn kick_peer(&self, slot: PeerSlot, code: AlertErrorCode) {
        if let Some(p) = self.peers.get(&slot) {
            let _ = p.tx.send(SessionToPeer::Shutdown);
        }
        self.alerts.push(Alert::Error { torrent: self.torrent_id, code });
    }

    fn maybe_express_interest(&mut self, slot: PeerSlot) {
        let Some(p) = self.peers.get_mut(&slot) else {
            return;
        };
        let want =
            p.have.iter().enumerate().any(|(i, has)| {
                *has && !self.picker.has_piece(u32::try_from(i).unwrap_or(u32::MAX))
            });
        if want != p.we_are_interested {
            p.we_are_interested = want;
            let _ = p.tx.send(SessionToPeer::SetInterested(want));
        }
    }

    fn schedule(&mut self) {
        if self.state == TorrentState::Completed {
            return;
        }
        // Greedy: walk peers, pick the rarest piece they have, queue blocks.
        // This is the M1 baseline. Endgame + BDP land later (ADR-0010).
        let slots: Vec<PeerSlot> = self.peers.keys().copied().collect();
        for slot in slots {
            loop {
                if !self.peer_can_request(slot) {
                    break;
                }
                if !self.assign_one_block(slot) {
                    break;
                }
            }
        }
    }

    fn peer_can_request(&self, slot: PeerSlot) -> bool {
        self.peers.get(&slot).is_some_and(PeerState::can_request)
    }

    /// Try to assign one block to `slot`. Returns false if no work was found.
    fn assign_one_block(&mut self, slot: PeerSlot) -> bool {
        let Some(peer) = self.peers.get(&slot) else {
            return false;
        };
        // Find a piece this peer has that we don't, and that isn't fully
        // claimed elsewhere. Iterate picker order via rarest-first by simply
        // scanning peers' bitfield for uncompleted pieces.
        let piece_count = self.params.piece_count;
        let mut chosen_piece = None;
        for piece in 0..piece_count {
            if !peer.have[piece as usize] {
                continue;
            }
            if self.picker.has_piece(piece) {
                continue;
            }
            // Has at least one unclaimed unreceived block?
            let in_progress = self.in_progress.get(&piece);
            let block_count = self.params.piece_size(piece).div_ceil(BLOCK_SIZE);
            let mut found = false;
            for idx in 0..block_count {
                let claimed = in_progress
                    .is_some_and(|p| p.claimed.get(idx as usize).is_some_and(Option::is_some));
                let received = in_progress
                    .is_some_and(|p| p.received.get(idx as usize).copied().unwrap_or(false));
                if !claimed && !received {
                    found = true;
                    break;
                }
            }
            if found {
                chosen_piece = Some(piece);
                break;
            }
        }
        let Some(piece) = chosen_piece else {
            return false;
        };
        let piece_size = self.params.piece_size(piece);
        let in_progress = self
            .in_progress
            .entry(piece)
            .or_insert_with(|| InProgressPiece::new(piece_size));
        // First unclaimed unreceived block.
        let mut chosen_idx = None;
        for idx in 0..in_progress.block_count {
            if !in_progress.received[idx as usize] && in_progress.claimed[idx as usize].is_none() {
                chosen_idx = Some(idx);
                break;
            }
        }
        let Some(idx) = chosen_idx else { return false };
        let block_size = InProgressPiece::block_size_at(idx, piece_size);
        in_progress.claimed[idx as usize] = Some(slot);
        let req = BlockRequest::new(piece, idx * BLOCK_SIZE, block_size);
        if let Some(p) = self.peers.get_mut(&slot) {
            p.in_flight += 1;
            let _ = p.tx.send(SessionToPeer::Request(req));
        }
        true
    }

    fn release_block_claim(&mut self, slot: PeerSlot, req: BlockRequest) {
        if let Some(p) = self.peers.get_mut(&slot) {
            p.in_flight = p.in_flight.saturating_sub(1);
        }
        let idx = req.offset / BLOCK_SIZE;
        if let Some(piece) = self.in_progress.get_mut(&req.piece)
            && let Some(slot_at) = piece.claimed.get_mut(idx as usize)
            && *slot_at == Some(slot)
        {
            *slot_at = None;
        }
    }

    fn release_claims(&mut self, slot: PeerSlot) {
        for piece in self.in_progress.values_mut() {
            for s in &mut piece.claimed {
                if *s == Some(slot) {
                    *s = None;
                }
            }
        }
    }

    async fn handle_block(&mut self, slot: PeerSlot, piece: u32, offset: u32, data: &Bytes) {
        // S6/S7: validate the (piece, offset) is in range and that *this peer*
        // was actually claimed for this block. Anything else is a protocol
        // violation or a duplicate from a parallel peer; either way, do not
        // mutate state and do not decrement in-flight.
        if piece >= self.params.piece_count {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }
        let piece_size = self.params.piece_size(piece);
        if !offset.is_multiple_of(BLOCK_SIZE) {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }
        let idx = offset / BLOCK_SIZE;
        let in_progress = self
            .in_progress
            .entry(piece)
            .or_insert_with(|| InProgressPiece::new(piece_size));
        let Some(received) = in_progress.received.get(idx as usize) else {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        };
        if *received {
            // Duplicate (likely benign — endgame, race). Don't mutate, don't
            // decrement in-flight: the original Request from this peer is
            // still considered satisfied, but a *second* arrival shouldn't
            // double-decrement.
            return;
        }
        let claimed_by = in_progress.claimed.get(idx as usize).copied().flatten();
        if claimed_by != Some(slot) {
            // S7: unsolicited Piece. Treat as protocol violation.
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }
        let end = (offset as usize) + data.len();
        if end > in_progress.buffer.len() {
            self.kick_peer(slot, AlertErrorCode::PeerProtocol);
            return;
        }
        // S8: only decrement in-flight after the block is accepted.
        if let Some(p) = self.peers.get_mut(&slot) {
            p.in_flight = p.in_flight.saturating_sub(1);
        }
        in_progress.buffer[offset as usize..end].copy_from_slice(data);
        in_progress.received[idx as usize] = true;
        in_progress.received_count += 1;
        in_progress.claimed[idx as usize] = None;
        if in_progress.received_count == in_progress.block_count {
            self.finalise_piece(piece).await;
        }
    }

    /// Enqueue a `DiskOp::VerifyAndWrite` for the given piece.
    ///
    /// The bounded `disk_tx` channel provides backpressure: when the
    /// `DiskWriter` is overwhelmed the actor stalls here, which in turn
    /// stalls peer-event processing, which in turn stalls peer tasks reading
    /// the wire. End-to-end TCP backpressure protects steady-state memory.
    ///
    /// We keep the [`InProgressPiece`] entry in place (with the buffer moved
    /// out) until the completion arrives — `received_count == block_count`
    /// means the scheduler treats it as "fully claimed", which prevents
    /// re-requesting a piece that's awaiting verification.
    async fn finalise_piece(&mut self, piece: u32) {
        let Some(in_progress) = self.in_progress.get_mut(&piece) else {
            return;
        };
        let buffer = Bytes::from(std::mem::take(&mut in_progress.buffer));
        let op = DiskOp::VerifyAndWrite {
            piece,
            offset: self.params.piece_offset(piece),
            buffer,
            expected_hash: self.params.piece_hash(piece),
            completion_tx: self.completion_tx.clone(),
        };
        if self.disk_tx.send(op).await.is_err() {
            // DiskWriter has gone away — drop the marker and surface I/O error.
            self.in_progress.remove(&piece);
            self.alerts.push(Alert::Error {
                torrent: self.torrent_id,
                code: AlertErrorCode::StorageIo,
            });
        }
    }
}

/// Encode our current bitfield into the wire format (high-bit first, spare
/// bits zero per BEP 3 §The peer wire protocol). Used by `handle_connected`
/// when the peer doesn't support BEP 6 fast-ext (no HaveAll/HaveNone).
fn pack_bitfield(picker: &Picker, piece_count: u32) -> Vec<u8> {
    let n_bytes = (piece_count as usize).div_ceil(8);
    let mut bytes = vec![0u8; n_bytes];
    for i in 0..piece_count {
        if picker.has_piece(i) {
            let byte = (i / 8) as usize;
            let bit = 7 - (i % 8) as usize;
            bytes[byte] |= 1 << bit;
        }
    }
    bytes
}

/// Errors returned by [`decode_bitfield_strict`].
#[derive(Debug)]
enum BitfieldError {
    /// Length did not match `ceil(piece_count / 8)`.
    BadLength,
    /// Spare bits in the final byte were non-zero (BEP 3 violation).
    BadSpareBits,
}

/// Decode a bitfield, enforcing BEP 3 length + spare-bit-zero invariants
/// (S4 + S5 hardening).
fn decode_bitfield_strict(bytes: &[u8], piece_count: u32) -> Result<Vec<bool>, BitfieldError> {
    let expected_len = piece_count.div_ceil(8) as usize;
    if bytes.len() != expected_len {
        return Err(BitfieldError::BadLength);
    }
    let extra_bits = (8 - piece_count % 8) % 8;
    if extra_bits != 0
        && let Some(last) = bytes.last()
    {
        let mask: u8 = (1u8 << extra_bits) - 1;
        if last & mask != 0 {
            return Err(BitfieldError::BadSpareBits);
        }
    }
    let mut out = vec![false; piece_count as usize];
    for (i, slot) in out.iter_mut().enumerate() {
        let byte = i / 8;
        let bit = 7 - (i % 8);
        *slot = (bytes[byte] >> bit) & 1 == 1;
    }
    Ok(out)
}

fn single_bit(piece: u32, len: usize) -> Vec<bool> {
    let mut v = vec![false; len];
    if (piece as usize) < len {
        v[piece as usize] = true;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_params() -> TorrentParams {
        TorrentParams {
            piece_count: 4,
            piece_length: 32 * 1024,
            total_length: 4 * 32 * 1024,
            piece_hashes: vec![0u8; 4 * 20],
            private: false,
        }
    }

    #[test]
    fn validate_accepts_well_formed() {
        assert!(ok_params().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_piece_count() {
        let mut p = ok_params();
        p.piece_count = 0;
        p.piece_hashes.clear();
        assert_eq!(p.validate(), Err(TorrentParamsError::ZeroPieceCount));
    }

    #[test]
    fn validate_rejects_zero_piece_length() {
        let mut p = ok_params();
        p.piece_length = 0;
        assert_eq!(p.validate(), Err(TorrentParamsError::ZeroPieceLength));
    }

    #[test]
    fn validate_rejects_zero_total_length() {
        let mut p = ok_params();
        p.total_length = 0;
        assert_eq!(p.validate(), Err(TorrentParamsError::ZeroTotalLength));
    }

    #[test]
    fn validate_rejects_mismatched_piece_hashes_length() {
        let mut p = ok_params();
        p.piece_hashes = vec![0u8; 4 * 20 - 1];
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            TorrentParamsError::PieceHashesLength {
                expected: 80,
                actual: 79
            }
        ));
    }

    #[test]
    fn validate_rejects_total_length_overflow() {
        let mut p = ok_params();
        p.total_length += 1;
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            TorrentParamsError::TotalLengthOverflow { .. }
        ));
    }

    #[test]
    fn bitfield_strict_accepts_well_formed() {
        // 13 pieces → 2 bytes. Spare 3 bits in the second byte must be zero.
        let bytes = [0b1111_1111, 0b1110_0000];
        let bits = decode_bitfield_strict(&bytes, 13).unwrap();
        assert_eq!(bits.len(), 13);
        assert!(bits.iter().take(11).all(|b| *b));
        assert!(!bits[11]);
        assert!(!bits[12]);
    }

    #[test]
    fn bitfield_strict_rejects_wrong_length() {
        // 13 pieces → expects 2 bytes; supply 3.
        let bytes = [0u8; 3];
        assert!(matches!(
            decode_bitfield_strict(&bytes, 13),
            Err(BitfieldError::BadLength)
        ));
        // Also rejects 1 byte.
        let bytes = [0u8; 1];
        assert!(matches!(
            decode_bitfield_strict(&bytes, 13),
            Err(BitfieldError::BadLength)
        ));
    }

    #[test]
    fn bitfield_strict_rejects_nonzero_spare_bits() {
        // 13 pieces; spare bits 5,6,7 of byte 1 must be zero. Set bit 0 → bad.
        let bytes = [0u8, 0b0000_0001];
        assert!(matches!(
            decode_bitfield_strict(&bytes, 13),
            Err(BitfieldError::BadSpareBits)
        ));
    }

    #[test]
    fn bitfield_strict_handles_byte_aligned_count() {
        // 16 pieces → 2 bytes, no spare bits.
        let bytes = [0xFF, 0xFF];
        assert!(decode_bitfield_strict(&bytes, 16).is_ok());
    }

    // --- ADR-0019 completion-transition ordering -----------------------------
    //
    // The five-step sequence under `completion_fired`:
    //   1. Alert::TorrentComplete
    //   2. choker swap + timer reset
    //   3. immediate seed-unchoke re-eval
    //   4. NotInterested broadcast (AFTER the unchoke round — so peers see our
    //      unchoke decision first, otherwise they drop Interested toward us and
    //      leave slots idle)
    //   5. fire-and-forget tracker event=completed
    //
    // Steps 2/3/5 are not yet wired into the actor (choker + tracker handles
    // land separately). The tests below pin the invariants that ARE observable
    // today: guard idempotency, alert-before-peer-message ordering, broadcast
    // scope (only to peers we are interested in), and the resume-from-complete
    // skip path. When 2/3/5 wire in, extend the test with:
    //   - assert choker-swap message is emitted between step 1 and step 4;
    //   - assert tracker event=completed is dispatched at step 5.

    fn adr_0019_params(piece_count: u32) -> TorrentParams {
        TorrentParams {
            piece_count,
            piece_length: 32 * 1024,
            total_length: u64::from(piece_count) * 32 * 1024,
            piece_hashes: vec![0u8; (piece_count as usize) * 20],
            private: false,
        }
    }

    /// Drain the post-`register_peer` initial Bitfield that the actor pushes
    /// onto every newly-registered peer's tx (sent so the remote learns what
    /// we have). Tests that look for specific subsequent messages must drop
    /// this first message before observing.
    fn drain_initial_advert(rx: &mut mpsc::UnboundedReceiver<SessionToPeer>) {
        match rx.try_recv() {
            Ok(SessionToPeer::SendBitfield(_)) => {}
            other => panic!("expected initial SendBitfield, got {other:?}"),
        }
    }

    fn adr_0019_session(params: TorrentParams) -> (TorrentSession, Arc<AlertQueue>) {
        let alerts = Arc::new(AlertQueue::new(16));
        let (_, peer_rx) = mpsc::channel(4);
        let (disk_tx, _disk_rx) = mpsc::channel(4);
        let read_cache = Arc::new(ReadCache::new(0));
        let (session, _cmd_tx) = TorrentSession::new(
            TorrentId::__test_new(1),
            params,
            [0u8; 20],
            Arc::clone(&alerts),
            peer_rx,
            disk_tx,
            read_cache,
            50, // peer_cap
        );
        (session, alerts)
    }

    #[test]
    fn adr_0019_transition_fires_alert_and_broadcasts_not_interested() {
        let (mut session, alerts) = adr_0019_session(adr_0019_params(1));

        // Register two peers. Peer A is we_are_interested; peer B is not.
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx_a));
        assert!(session.register_peer(PeerSlot(2), tx_b));
        drain_initial_advert(&mut rx_a);
        drain_initial_advert(&mut rx_b);
        session
            .peers
            .get_mut(&PeerSlot(1))
            .unwrap()
            .we_are_interested = true;

        session.maybe_fire_completion_transition();

        // Guard is set.
        assert!(session.completion_fired);

        // Step 1: exactly one TorrentComplete alert.
        let drained = alerts.drain();
        assert_eq!(drained.len(), 1, "exactly one alert");
        assert!(matches!(drained[0], Alert::TorrentComplete { .. }));

        // Step 4: NotInterested broadcast — only to the interested peer.
        let msg = rx_a
            .try_recv()
            .expect("interested peer receives SetInterested(false)");
        assert!(matches!(msg, SessionToPeer::SetInterested(false)));
        assert!(rx_a.try_recv().is_err(), "no further message to peer A");
        assert!(
            rx_b.try_recv().is_err(),
            "peer B was not interested — must not receive NotInterested"
        );

        // Flag is flipped on the peer.
        assert!(!session.peers.get(&PeerSlot(1)).unwrap().we_are_interested);
    }

    #[test]
    fn adr_0019_transition_is_idempotent_under_completion_fired() {
        let (mut session, alerts) = adr_0019_session(adr_0019_params(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        session
            .peers
            .get_mut(&PeerSlot(1))
            .unwrap()
            .we_are_interested = true;

        session.maybe_fire_completion_transition();
        let first = alerts.drain();
        assert_eq!(first.len(), 1);
        let _ = rx.try_recv().expect("first call sent NotInterested");

        // Second call: guard must short-circuit. No duplicate alert, no
        // duplicate peer message. Peer A was reset on the first call, so even
        // if the guard were missing the peer loop would be a no-op — to
        // meaningfully exercise the guard we re-arm we_are_interested and
        // verify the second call still emits nothing.
        session
            .peers
            .get_mut(&PeerSlot(1))
            .unwrap()
            .we_are_interested = true;

        session.maybe_fire_completion_transition();
        assert!(
            alerts.drain().is_empty(),
            "guard must prevent duplicate TorrentComplete"
        );
        assert!(
            rx.try_recv().is_err(),
            "guard must prevent duplicate NotInterested even if peer re-armed"
        );
        // Peer's flag must NOT be flipped again — guard short-circuits before
        // the broadcast loop touches peers.
        assert!(
            session.peers.get(&PeerSlot(1)).unwrap().we_are_interested,
            "guard short-circuits before peer-state mutation"
        );
    }

    #[test]
    fn adr_0019_resume_from_complete_skips_transition() {
        let (mut session, alerts) = adr_0019_session(adr_0019_params(2));
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        session
            .peers
            .get_mut(&PeerSlot(1))
            .unwrap()
            .we_are_interested = true;

        // Load in fully-complete — emulates resume-from-disk where every
        // piece is already verified.
        session.apply_initial_have(&[true, true]);

        assert!(
            session.completion_fired,
            "resume path pre-sets the guard so no transition fires"
        );
        assert_eq!(session.state, TorrentState::Completed);
        assert!(
            alerts.drain().is_empty(),
            "resume must not emit TorrentComplete"
        );
        assert!(
            rx.try_recv().is_err(),
            "resume must not broadcast NotInterested"
        );
        // Any subsequent completion-fire call is still a no-op.
        session.maybe_fire_completion_transition();
        assert!(alerts.drain().is_empty());
        assert!(rx.try_recv().is_err());
    }

    // --- G1 pause/resume actor-level invariants -----------------------------

    fn g1_session() -> (TorrentSession, Arc<AlertQueue>) {
        adr_0019_session(adr_0019_params(2))
    }

    #[test]
    fn g1_pause_chokes_unchoked_peers_and_keeps_them_choked() {
        let (mut session, _alerts) = g1_session();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx_a));
        assert!(session.register_peer(PeerSlot(2), tx_b));
        drain_initial_advert(&mut rx_a);
        drain_initial_advert(&mut rx_b);
        // Peer A starts already-unchoked (e.g. mid-serve); peer B already choked.
        session.peers.get_mut(&PeerSlot(1)).unwrap().we_choking = false;
        // (B keeps default we_choking = true)

        session.set_paused(true);
        assert!(session.paused);
        // A receives Choke; B receives nothing (already choked).
        assert!(matches!(rx_a.try_recv(), Ok(SessionToPeer::Choke)));
        assert!(rx_a.try_recv().is_err(), "A receives at most one Choke");
        assert!(rx_b.try_recv().is_err(), "B was already choked, no message");
        assert!(session.peers.get(&PeerSlot(1)).unwrap().we_choking);
    }

    #[test]
    fn g1_resume_unchokes_only_interested_peers() {
        let (mut session, _alerts) = g1_session();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx_a));
        assert!(session.register_peer(PeerSlot(2), tx_b));
        drain_initial_advert(&mut rx_a);
        drain_initial_advert(&mut rx_b);

        // Pre-pause: A is interested, B is not. Both choked (default).
        session.peers.get_mut(&PeerSlot(1)).unwrap().peer_interested = true;
        session.set_paused(true);
        let _ = rx_a.try_recv(); // drain initial pause-time messages (none expected here)
        let _ = rx_b.try_recv();

        session.set_paused(false);
        assert!(!session.paused);
        // A receives Unchoke (was interested and choked); B receives nothing.
        assert!(matches!(rx_a.try_recv(), Ok(SessionToPeer::Unchoke)));
        assert!(rx_a.try_recv().is_err());
        assert!(
            rx_b.try_recv().is_err(),
            "B not interested, must stay choked"
        );
        assert!(!session.peers.get(&PeerSlot(1)).unwrap().we_choking);
        assert!(session.peers.get(&PeerSlot(2)).unwrap().we_choking);
    }

    #[test]
    fn g1_pause_is_idempotent_no_duplicate_choke() {
        let (mut session, _alerts) = g1_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        session.peers.get_mut(&PeerSlot(1)).unwrap().we_choking = false;

        session.set_paused(true);
        assert!(matches!(rx.try_recv(), Ok(SessionToPeer::Choke)));
        // Second pause: no-op.
        session.set_paused(true);
        assert!(rx.try_recv().is_err(), "idempotent pause must not re-choke");
    }

    #[test]
    fn initial_advert_uses_have_all_for_complete_seed_with_fast_ext() {
        // #21: when supports_fast=true and we have every piece, send
        // HaveAll (1 byte on wire) instead of a full Bitfield.
        let (mut session, _alerts) = adr_0019_session(adr_0019_params(4));
        // Mark every piece as-have so missing_count == 0.
        for i in 0..4 {
            session.picker.mark_have(i);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer_with(PeerSlot(1), tx, DEFAULT_PER_PEER_IN_FLIGHT, true));
        let msg = rx.try_recv().expect("first advert");
        assert!(matches!(msg, SessionToPeer::SendHaveAll), "got {msg:?}");
    }

    #[test]
    fn initial_advert_uses_have_none_for_empty_leech_with_fast_ext() {
        // #21: when supports_fast=true and we have no pieces, send
        // HaveNone instead of a zero-filled Bitfield.
        let (mut session, _alerts) = adr_0019_session(adr_0019_params(4));
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer_with(PeerSlot(1), tx, DEFAULT_PER_PEER_IN_FLIGHT, true));
        let msg = rx.try_recv().expect("first advert");
        assert!(matches!(msg, SessionToPeer::SendHaveNone), "got {msg:?}");
    }

    #[test]
    fn initial_advert_falls_back_to_bitfield_without_fast_ext() {
        // #21: when supports_fast=false, always send Bitfield — BEP 3
        // is universal. Guards against a regression that would break
        // interop with non-fast peers.
        let (mut session, _alerts) = adr_0019_session(adr_0019_params(4));
        for i in 0..4 {
            session.picker.mark_have(i);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer_with(PeerSlot(1), tx, DEFAULT_PER_PEER_IN_FLIGHT, false));
        let msg = rx.try_recv().expect("first advert");
        assert!(matches!(msg, SessionToPeer::SendBitfield(_)), "got {msg:?}");
    }

    #[test]
    fn initial_advert_uses_bitfield_for_partial_have_even_with_fast_ext() {
        // #21: HaveAll/HaveNone shortcuts only apply at the extremes.
        // Partial `have` must use Bitfield.
        let (mut session, _alerts) = adr_0019_session(adr_0019_params(4));
        session.picker.mark_have(1);
        session.picker.mark_have(3);
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer_with(PeerSlot(1), tx, DEFAULT_PER_PEER_IN_FLIGHT, true));
        let msg = rx.try_recv().expect("first advert");
        assert!(matches!(msg, SessionToPeer::SendBitfield(_)), "got {msg:?}");
    }

    #[test]
    fn g1_pause_cancels_outstanding_requests_and_releases_claims() {
        // #20 tightening: when pause fires, any outstanding outbound
        // Request must be canceled (Cancel sent) and its claim released
        // so a subsequent resume+schedule pass can re-request fresh.
        let (mut session, _alerts) = g1_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        // Fabricate an in-progress claim: piece 0, block 0, claimed by
        // peer 1. This matches what `assign_one_block` would set up.
        let piece_size = 32 * 1024_u32;
        let mut in_prog = InProgressPiece::new(piece_size);
        in_prog.claimed[0] = Some(PeerSlot(1));
        session.in_progress.insert(0, in_prog);

        session.set_paused(true);
        // Step A: Choke broadcast (peer started unchoked-by-default? no —
        // default is we_choking=true, so no Choke message). Keep only the
        // Cancel in scope.
        let mut saw_cancel = false;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, SessionToPeer::Cancel(_)) {
                saw_cancel = true;
            }
        }
        assert!(
            saw_cancel,
            "pause must broadcast Cancel for the claimed block"
        );
        // Claim must be released so a future resume can re-request.
        let in_prog = session.in_progress.get(&0).expect("piece still tracked");
        assert!(
            in_prog.claimed[0].is_none(),
            "pause must release the peer's block claim"
        );
    }

    #[test]
    fn g1_paused_blocks_auto_unchoke_on_new_interest() {
        let (mut session, _alerts) = g1_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        // Need we_choking=true (default) so a new-interest event would
        // normally trigger an auto-unchoke.

        session.set_paused(true);
        let _ = rx.try_recv();
        // Now a peer becomes interested. Pre-G1 the actor would auto-unchoke.
        session.handle_peer_interest(PeerSlot(1), true);
        assert!(rx.try_recv().is_err(), "paused must suppress auto-unchoke");
        assert!(session.peers.get(&PeerSlot(1)).unwrap().we_choking);

        // After resume, the broadcast loop unchokes interested peers.
        session.set_paused(false);
        assert!(matches!(rx.try_recv(), Ok(SessionToPeer::Unchoke)));
    }

    #[test]
    fn adr_0019_alert_ordered_before_peer_broadcast() {
        // Ordering check: the alert lands in the queue BEFORE any peer
        // message is dispatched. Today the implementation pushes the alert
        // synchronously and then loops peers in a single function; this test
        // pins that ordering so a future refactor (e.g. deferring the alert
        // behind an await) does not silently flip step 1 past step 4.
        let (mut session, alerts) = adr_0019_session(adr_0019_params(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(session.register_peer(PeerSlot(1), tx));
        drain_initial_advert(&mut rx);
        session
            .peers
            .get_mut(&PeerSlot(1))
            .unwrap()
            .we_are_interested = true;

        session.maybe_fire_completion_transition();

        // Drain in the same order the actor produced: alert first, then peer
        // message. If step 4 ever lands before step 1 the alert drain would be
        // empty here while the peer rx already has a message.
        let drained = alerts.drain();
        assert_eq!(drained.len(), 1, "alert must be queued before broadcast");
        assert!(matches!(drained[0], Alert::TorrentComplete { .. }));
        assert!(
            matches!(rx.try_recv(), Ok(SessionToPeer::SetInterested(false))),
            "NotInterested broadcast follows the alert"
        );
    }
}
