//! Per-peer upload request queue (ADR-0017).
//!
//! A peer that asks us for blocks maintains two virtual queues inside the
//! peer actor:
//!
//! 1. **Unread queue**: requests we've received from the peer but haven't
//!    dispatched to disk yet. Cap 128 with drop-newest on overflow
//!    (rasterbar `max_allowed_in_request_queue` lineage) — rejects abuse
//!    without disconnecting well-behaved peers that briefly pipeline hard.
//!
//! 2. **Ready queue**: block payloads loaded from disk (or cache), awaiting
//!    send on the wire. Managed elsewhere via the adaptive
//!    `send_buffer_watermark` — not modelled here.
//!
//! ## Watermark
//!
//! Adaptive send-buffer watermark (bytes): `clamp(rate × 0.5 s, 128 KiB,
//! 4 MiB)`. When the per-peer ready-queue bytes reach this ceiling, the
//! peer stops submitting new disk reads. Prevents a fast peer from pinning
//! an unbounded slice of the read cache.
//!
//! ## Choke semantics
//!
//! On choke (BEP 3) the unread queue empties. BEP 6 Fast Extension changes
//! this: survivors are tracked via the `allowed_fast` set, abuse bounded to
//! `3 × blocks_per_piece` per piece while choked.
//!
//! ## Post-choke grace
//!
//! 2 s after we choke a peer, we tolerate already-in-flight requests.
//! Beyond the grace window, repeated requests from a choked peer are
//! counted as abuse and the peer is disconnected.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use magpie_bt_wire::{BLOCK_SIZE, BlockRequest};

/// Cap on pending unread requests per peer (ADR-0017).
pub const DEFAULT_UNREAD_QUEUE_CAP: usize = 128;

/// Minimum adaptive send-buffer watermark (ADR-0017).
pub const WATERMARK_MIN_BYTES: u32 = 128 * 1024;

/// Maximum adaptive send-buffer watermark.
pub const WATERMARK_MAX_BYTES: u32 = 4 * 1024 * 1024;

/// Adaptive watermark horizon: `rate × WATERMARK_HORIZON`.
pub const WATERMARK_HORIZON: Duration = Duration::from_millis(500);

/// Grace window after choking a peer during which their in-flight requests
/// are tolerated (ADR-0017 §post-choke).
pub const POST_CHOKE_GRACE: Duration = Duration::from_secs(2);

/// Fast-set abuse cap: while choked, a peer may re-request `N × blocks_per_piece`
/// worth of allowed-fast blocks before we disconnect them. `N = 3` per ADR-0017.
pub const FAST_SET_ABUSE_MULTIPLIER: u32 = 3;

/// Outcome of [`PeerUploadQueue::accept_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// Request enqueued and ready for dispatch.
    Queued,
    /// Queue was full; newest request dropped silently. Peer sees no reject;
    /// the request simply never produces a block.
    DroppedNewest,
    /// We're choking the peer and the request is neither inside the
    /// `POST_CHOKE_GRACE` window nor in our `allowed_fast` set. Caller
    /// should reply with `RejectRequest` (fast-ext) or disconnect.
    RejectedChoked,
    /// Peer has exceeded the fast-set abuse cap while choked. Caller must
    /// disconnect.
    FastSetAbuse,
    /// Duplicate in-flight request. Silently ignored.
    Duplicate,
}

/// Per-peer upload queue.
///
/// Single-threaded: owned by the peer actor, not shared. Decisions that
/// require torrent-actor state (e.g. do we `has_piece`?) happen upstream;
/// this type just enforces the local queue invariants.
#[derive(Debug)]
pub struct PeerUploadQueue {
    queue: VecDeque<BlockRequest>,
    in_flight: HashSet<BlockRequest>,
    allowed_fast: HashSet<u32>,
    fast_set_uses: u32,
    /// When we last choked the peer. `None` means we are not choking them.
    choked_at: Option<Instant>,
    /// Current ready-queue byte count; updated by the caller on send + ack.
    ready_bytes: u32,
    /// Smoothed upload rate to this peer (bytes/sec), for watermark.
    upload_rate_bps: u64,
    /// Blocks-per-piece for this torrent, used to bound the fast-set abuse
    /// cap.
    blocks_per_piece: u32,
    cap: usize,
}

impl PeerUploadQueue {
    /// Construct a queue sized for a torrent with `blocks_per_piece` blocks
    /// per piece. Typical: `piece_length.div_ceil(BLOCK_SIZE)`.
    #[must_use]
    pub fn new(blocks_per_piece: u32) -> Self {
        Self::with_cap(blocks_per_piece, DEFAULT_UNREAD_QUEUE_CAP)
    }

    /// Construct with an explicit cap (for tests).
    #[must_use]
    pub fn with_cap(blocks_per_piece: u32, cap: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            in_flight: HashSet::new(),
            allowed_fast: HashSet::new(),
            fast_set_uses: 0,
            choked_at: None,
            ready_bytes: 0,
            upload_rate_bps: 0,
            blocks_per_piece: blocks_per_piece.max(1),
            cap,
        }
    }

    /// Try to accept an inbound `Request` from the peer.
    pub fn accept_request(&mut self, req: BlockRequest) -> AcceptOutcome {
        // Duplicate in-flight or queued.
        if self.in_flight.contains(&req) || self.queue.contains(&req) {
            return AcceptOutcome::Duplicate;
        }
        // Choke handling.
        if let Some(choked_at) = self.choked_at {
            if choked_at.elapsed() < POST_CHOKE_GRACE {
                // Within grace: accept as if unchoked.
            } else if self.allowed_fast.contains(&req.piece) {
                let cap = self
                    .blocks_per_piece
                    .saturating_mul(FAST_SET_ABUSE_MULTIPLIER);
                if self.fast_set_uses >= cap {
                    return AcceptOutcome::FastSetAbuse;
                }
                self.fast_set_uses += 1;
            } else {
                return AcceptOutcome::RejectedChoked;
            }
        }
        if self.queue.len() + self.in_flight.len() >= self.cap {
            return AcceptOutcome::DroppedNewest;
        }
        self.queue.push_back(req);
        AcceptOutcome::Queued
    }

    /// Peek + pop a request ready for disk dispatch. Caller moves the result
    /// into the ready-queue path (read cache / disk). The request transitions
    /// to "in-flight" until [`PeerUploadQueue::release`] is called.
    pub fn dispatch_next(&mut self) -> Option<BlockRequest> {
        let req = self.queue.pop_front()?;
        self.in_flight.insert(req);
        Some(req)
    }

    /// Mark an in-flight request as completed (block sent to the peer or
    /// abandoned). Caller passes the same request back.
    pub fn release(&mut self, req: &BlockRequest) {
        self.in_flight.remove(req);
    }

    /// Peer sent `Cancel`. Remove from both queue and in-flight.
    pub fn cancel(&mut self, req: &BlockRequest) {
        self.queue.retain(|r| r != req);
        self.in_flight.remove(req);
    }

    /// We sent `Choke` to the peer. Drop the unread queue (BEP 3) unless
    /// fast-ext is in use (callers can instead transition items to the fast
    /// set by updating `allowed_fast` before this call).
    pub fn on_self_choke(&mut self, use_fast_ext: bool) {
        self.choked_at = Some(Instant::now());
        if !use_fast_ext {
            self.queue.clear();
            self.in_flight.clear();
            return;
        }
        // Fast ext: only non-allowed-fast entries drop.
        self.queue.retain(|r| self.allowed_fast.contains(&r.piece));
        self.in_flight
            .retain(|r| self.allowed_fast.contains(&r.piece));
    }

    /// We sent `Unchoke` to the peer. Reset grace + abuse counters.
    pub const fn on_self_unchoke(&mut self) {
        self.choked_at = None;
        self.fast_set_uses = 0;
    }

    /// Update our smoothed upload rate (bytes/sec) for watermark calculation.
    pub const fn set_upload_rate(&mut self, bps: u64) {
        self.upload_rate_bps = bps;
    }

    /// Add a piece to the `allowed_fast` set (BEP 6).
    pub fn add_allowed_fast(&mut self, piece: u32) {
        self.allowed_fast.insert(piece);
    }

    /// Current adaptive watermark in bytes.
    #[must_use]
    pub fn watermark_bytes(&self) -> u32 {
        let horizon_ms = u64::try_from(WATERMARK_HORIZON.as_millis()).unwrap_or(u64::MAX);
        // bps × 0.5 s = (bps × 500) / 1000  (div by 1000 → ms)
        let target = self.upload_rate_bps.saturating_mul(horizon_ms) / 1000;
        let target = u32::try_from(target).unwrap_or(WATERMARK_MAX_BYTES);
        target.clamp(WATERMARK_MIN_BYTES, WATERMARK_MAX_BYTES)
    }

    /// Can the caller submit another disk read without exceeding the
    /// watermark? Call before `dispatch_next`.
    #[must_use]
    pub fn can_submit_read(&self, block_bytes: u32) -> bool {
        self.ready_bytes.saturating_add(block_bytes) <= self.watermark_bytes()
    }

    /// Adjust the ready-queue byte count when a block is added to the
    /// outgoing buffer.
    pub const fn add_ready(&mut self, bytes: u32) {
        self.ready_bytes = self.ready_bytes.saturating_add(bytes);
    }

    /// Adjust the ready-queue byte count when a block is written to the wire.
    pub const fn remove_ready(&mut self, bytes: u32) {
        self.ready_bytes = self.ready_bytes.saturating_sub(bytes);
    }

    /// Number of unread requests sitting in the queue.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Number of dispatched-but-not-yet-released in-flight requests.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Total queue depth (unread + in-flight). Bounded by `cap`.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.queue.len() + self.in_flight.len()
    }
}

/// Typical block count for a standard 256 KiB piece. Helper for call sites
/// that don't yet plumb per-torrent piece geometry.
#[must_use]
pub const fn default_blocks_per_piece(piece_length: u32) -> u32 {
    piece_length.div_ceil(BLOCK_SIZE)
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unchecked_time_subtraction
)]
mod tests {
    use super::*;

    fn req(piece: u32, offset: u32) -> BlockRequest {
        BlockRequest::new(piece, offset, BLOCK_SIZE)
    }

    #[test]
    fn unchoked_queue_accepts_up_to_cap() {
        let mut q = PeerUploadQueue::with_cap(16, 3);
        assert_eq!(q.accept_request(req(0, 0)), AcceptOutcome::Queued);
        assert_eq!(q.accept_request(req(0, BLOCK_SIZE)), AcceptOutcome::Queued);
        assert_eq!(
            q.accept_request(req(0, 2 * BLOCK_SIZE)),
            AcceptOutcome::Queued
        );
        assert_eq!(q.accept_request(req(1, 0)), AcceptOutcome::DroppedNewest);
    }

    #[test]
    fn duplicate_request_rejected_silently() {
        let mut q = PeerUploadQueue::new(16);
        assert_eq!(q.accept_request(req(0, 0)), AcceptOutcome::Queued);
        assert_eq!(q.accept_request(req(0, 0)), AcceptOutcome::Duplicate);
    }

    #[test]
    fn in_flight_duplicate_also_rejected() {
        let mut q = PeerUploadQueue::new(16);
        q.accept_request(req(0, 0));
        let r = q.dispatch_next().unwrap();
        assert_eq!(r, req(0, 0));
        // Same request in flight; peer re-asks → duplicate.
        assert_eq!(q.accept_request(req(0, 0)), AcceptOutcome::Duplicate);
    }

    #[test]
    fn choke_clears_queue_without_fast_ext() {
        let mut q = PeerUploadQueue::new(16);
        q.accept_request(req(0, 0));
        q.accept_request(req(1, 0));
        q.on_self_choke(false);
        assert_eq!(q.queued(), 0);
        assert_eq!(q.in_flight_len(), 0);
    }

    #[test]
    fn fast_ext_choke_retains_allowed_fast() {
        let mut q = PeerUploadQueue::new(16);
        q.add_allowed_fast(3);
        q.accept_request(req(0, 0));
        q.accept_request(req(3, 0)); // allowed-fast
        q.on_self_choke(true);
        assert_eq!(q.queued(), 1);
    }

    #[test]
    fn fast_set_abuse_triggers_after_cap() {
        // blocks_per_piece=4, cap=3*4=12 re-requests while choked.
        // Advance choked_at past the grace window so the fast-set path
        // activates.
        let mut q = PeerUploadQueue::with_cap(4, 256);
        q.add_allowed_fast(0);
        q.on_self_choke(true);
        q.choked_at = Some(Instant::now() - Duration::from_secs(3));
        // 12 accepts, then abuse.
        for i in 0..12 {
            let got = q.accept_request(req(0, (i as u32) * BLOCK_SIZE));
            assert_eq!(got, AcceptOutcome::Queued, "iteration {i}");
        }
        let abusive = q.accept_request(req(0, 12 * BLOCK_SIZE));
        assert_eq!(abusive, AcceptOutcome::FastSetAbuse);
    }

    #[test]
    fn post_choke_grace_accepts_for_2s() {
        let mut q = PeerUploadQueue::new(16);
        q.on_self_choke(false); // choke with no fast ext
        // Immediately after choke, a straggler request is within grace.
        let outcome = q.accept_request(req(7, 0));
        assert_eq!(outcome, AcceptOutcome::Queued);
    }

    #[test]
    fn choked_non_fast_rejected_outside_grace() {
        // We can't easily time-travel 2s in a unit test; simulate by
        // mutating the `choked_at` timestamp directly.
        let mut q = PeerUploadQueue::new(16);
        q.on_self_choke(false);
        q.choked_at = Some(Instant::now() - Duration::from_secs(3));
        let outcome = q.accept_request(req(7, 0));
        assert_eq!(outcome, AcceptOutcome::RejectedChoked);
    }

    #[test]
    fn watermark_clamps_to_min_at_zero_rate() {
        let q = PeerUploadQueue::new(16);
        assert_eq!(q.watermark_bytes(), WATERMARK_MIN_BYTES);
    }

    #[test]
    fn watermark_scales_with_rate() {
        let mut q = PeerUploadQueue::new(16);
        q.set_upload_rate(2 * 1024 * 1024); // 2 MiB/s
        // rate × 0.5 s = 1 MiB → within min/max range.
        assert_eq!(q.watermark_bytes(), 1024 * 1024);
    }

    #[test]
    fn watermark_clamps_to_max_for_high_rate() {
        let mut q = PeerUploadQueue::new(16);
        q.set_upload_rate(100 * 1024 * 1024); // 100 MiB/s
        assert_eq!(q.watermark_bytes(), WATERMARK_MAX_BYTES);
    }

    #[test]
    fn can_submit_read_respects_watermark() {
        let mut q = PeerUploadQueue::new(16);
        // Min watermark = 128 KiB.
        assert!(q.can_submit_read(BLOCK_SIZE));
        q.add_ready(WATERMARK_MIN_BYTES);
        assert!(!q.can_submit_read(1));
    }

    #[test]
    fn cancel_removes_from_both_queues() {
        let mut q = PeerUploadQueue::new(16);
        q.accept_request(req(0, 0));
        q.accept_request(req(1, 0));
        let _ = q.dispatch_next();
        q.cancel(&req(0, 0));
        q.cancel(&req(1, 0));
        assert_eq!(q.queued(), 0);
        assert_eq!(q.in_flight_len(), 0);
    }
}
