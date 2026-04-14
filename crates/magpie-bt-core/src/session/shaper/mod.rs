//! Three-tier bandwidth shaper (ADR-0013).
//!
//! Hierarchical token-bucket: session ↔ per-torrent ↔ per-peer. Six
//! buckets per session (up + down at each tier).
//!
//! **Consume-on-wire**: peer bucket checked per send/recv (two atomics per
//! block — `try_consume`'s `fetch_sub` + demand bookkeeping).
//!
//! **Refill cadence**: 100 ms. Session + torrent tiers are *only* touched
//! by the refiller. Peer tier is touched on every wire event, and gets its
//! tokens from the per-tick refill grant.
//!
//! **Proportional-to-demand grant**: parent tier measures each child's
//! `(consumed + denied)` since the last tick and grants the available token
//! budget in proportion.
//!
//! **Pass-through at `u64::MAX`** is explicitly load-bearing (plan invariant
//! #3). When any tier is configured at the max rate — which is the M2
//! default — the bucket *still participates* in the refill cycle. This
//! means the three-tier path is exercised from day one, so M5 cap-enablement
//! is a config change rather than a refactor. The
//! `refiller_touches_all_three_tiers_even_at_passthrough` unit test
//! (in `refiller::tests`) enforces this invariant.

#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::TorrentId;
use crate::session::messages::PeerSlot;

pub mod bucket;
pub mod refiller;

pub use bucket::TokenBucket;
pub use refiller::{REFILL_INTERVAL, Refiller};

/// Pass-through rate — no effective limit, but still participates in refill.
pub const PASSTHROUGH_RATE: u64 = u64::MAX;

/// Default bucket capacity (token headroom beyond per-tick grant). Sized at
/// 64 KiB × max blocks to absorb bursts without starving.
pub const DEFAULT_CAPACITY: u64 = 4 * 1024 * 1024;

/// Up + down buckets at a given tier.
#[derive(Debug)]
pub struct DuplexBuckets {
    /// Upload-direction bucket.
    pub up: TokenBucket,
    /// Download-direction bucket.
    pub down: TokenBucket,
}

impl DuplexBuckets {
    /// Construct a fresh duplex pair.
    #[must_use]
    pub fn new(up_rate_bps: u64, down_rate_bps: u64) -> Self {
        Self {
            up: TokenBucket::new(up_rate_bps, DEFAULT_CAPACITY),
            down: TokenBucket::new(down_rate_bps, DEFAULT_CAPACITY),
        }
    }

    /// Construct a pass-through duplex pair (no effective cap). Both
    /// directions still participate in the refill cycle.
    #[must_use]
    pub fn passthrough() -> Self {
        Self::new(PASSTHROUGH_RATE, PASSTHROUGH_RATE)
    }
}

/// Per-torrent tier entry.
#[derive(Debug)]
pub struct TorrentBuckets {
    /// The duplex bucket pair for this torrent.
    pub buckets: DuplexBuckets,
}

/// Per-peer tier entry.
///
/// `buckets` is `Arc<DuplexBuckets>` so peer tasks can cache a clone of
/// the handle at startup and call `try_consume` without touching the
/// `Shaper::peers` mutex on the hot path. The Refiller touches this Arc
/// once per tick under the same mutex; peer tasks hit it lock-free.
#[derive(Debug)]
pub struct PeerBuckets {
    /// The duplex bucket pair for this peer.
    pub buckets: Arc<DuplexBuckets>,
    /// Parent torrent — refiller needs this to route grants.
    pub torrent_id: TorrentId,
}

/// Three-tier shaper. Owned by the engine; cloned via [`std::sync::Arc`]
/// into peer tasks and the refiller.
#[derive(Debug)]
pub struct Shaper {
    /// Session (global) tier.
    pub session: DuplexBuckets,
    /// Per-torrent tier.
    pub torrents: Mutex<HashMap<TorrentId, TorrentBuckets>>,
    /// Per-peer tier.
    pub peers: Mutex<HashMap<PeerSlot, PeerBuckets>>,
    /// Monotonic tick counter (incremented by the refiller). Exposed for
    /// tests asserting the refill path ran.
    pub refill_ticks: AtomicU64,
}

impl Shaper {
    /// Construct with pass-through session defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session: DuplexBuckets::passthrough(),
            torrents: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            refill_ticks: AtomicU64::new(0),
        }
    }

    /// Register a torrent's bucket pair. Idempotent: if the torrent is
    /// already registered, its bucket pair is replaced.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn register_torrent(&self, torrent_id: TorrentId, buckets: DuplexBuckets) {
        self.torrents
            .lock()
            .expect("shaper torrents poisoned")
            .insert(torrent_id, TorrentBuckets { buckets });
    }

    /// Register a torrent at pass-through rate. Shortcut for
    /// [`Self::register_torrent`] with [`DuplexBuckets::passthrough`].
    pub fn register_torrent_passthrough(&self, torrent_id: TorrentId) {
        self.register_torrent(torrent_id, DuplexBuckets::passthrough());
    }

    /// Register a peer's bucket pair.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn register_peer(&self, slot: PeerSlot, torrent_id: TorrentId, buckets: DuplexBuckets) {
        self.peers.lock().expect("shaper peers poisoned").insert(
            slot,
            PeerBuckets {
                buckets: Arc::new(buckets),
                torrent_id,
            },
        );
    }

    /// Look up a peer's bucket handle. Returns a cheap `Arc` clone so peer
    /// tasks can cache it at startup and call `try_consume` on the hot
    /// path without re-entering the mutex. `None` if `slot` was not
    /// registered (e.g. race against `drop_peer`).
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    #[must_use]
    pub fn peer_buckets(&self, slot: PeerSlot) -> Option<Arc<DuplexBuckets>> {
        self.peers
            .lock()
            .expect("shaper peers poisoned")
            .get(&slot)
            .map(|pb| Arc::clone(&pb.buckets))
    }

    /// Drop a peer when its connection closes.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn drop_peer(&self, slot: PeerSlot) {
        self.peers
            .lock()
            .expect("shaper peers poisoned")
            .remove(&slot);
    }

    /// Drop a torrent at shutdown.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn drop_torrent(&self, torrent_id: TorrentId) {
        self.torrents
            .lock()
            .expect("shaper torrents poisoned")
            .remove(&torrent_id);
        // Drop any peers still keyed to this torrent.
        self.peers
            .lock()
            .expect("shaper peers poisoned")
            .retain(|_, p| p.torrent_id != torrent_id);
    }

    /// Current refill tick count. Used by tests asserting the refill path
    /// has been exercised.
    #[must_use]
    pub fn refill_ticks(&self) -> u64 {
        self.refill_ticks.load(Ordering::Relaxed)
    }
}

impl Default for Shaper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unchecked_time_subtraction)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn mk_torrent_id(n: u64) -> TorrentId {
        // TorrentId has a private ctor, so we use the public Engine::add_torrent
        // pattern indirectly via a transmute-free cheat: the only field is u64.
        // Since this is a test file within the crate, we can use unsafe; but
        // we're `deny(unsafe_code)`, so construct through Engine in a full
        // integration test and just use index-fake via Debug equality here.
        let _ = n;
        // Fall back: synthesize a TorrentId from a size-of-u64 pattern. Since
        // TorrentId's u64 field is private, we can't construct directly.
        // Use Engine to get real ids — but this is a lib-level test, not
        // integration. Skip tests that need real TorrentIds by checking
        // shaper behavior through peer slots only.
        TorrentId::__test_new(n)
    }

    #[test]
    fn refiller_touches_all_three_tiers_even_at_passthrough() {
        // Plan invariant #3: pass-through buckets must still participate in
        // the refill cycle. Enforced by counting ticks after one refill pass.
        let shaper = Arc::new(Shaper::new());
        let tid = mk_torrent_id(0);
        shaper.register_torrent_passthrough(tid);
        shaper.register_peer(PeerSlot(0), tid, DuplexBuckets::passthrough());

        let before = shaper.refill_ticks();
        refiller::run_one_tick(&shaper, Duration::from_millis(100));
        let after = shaper.refill_ticks();
        assert_eq!(after, before + 1, "refiller must advance the tick counter");

        // All three tiers must show evidence of the refill pass. With
        // pass-through rates, `try_consume` always succeeds and tokens stay
        // at u64::MAX. We assert the bucket's "last refill" accounting
        // instead — exposed via TokenBucket::refill_ticks.
        assert!(shaper.session.up.refill_ticks() >= 1);
        assert!(shaper.session.down.refill_ticks() >= 1);
        let peers = shaper.peers.lock().unwrap();
        let peer = peers.get(&PeerSlot(0)).unwrap();
        assert!(peer.buckets.up.refill_ticks() >= 1);
        assert!(peer.buckets.down.refill_ticks() >= 1);
        let torrents = shaper.torrents.lock().unwrap();
        let torrent = torrents.get(&tid).unwrap();
        assert!(torrent.buckets.up.refill_ticks() >= 1);
        assert!(torrent.buckets.down.refill_ticks() >= 1);
    }

    #[test]
    fn drop_torrent_removes_peers() {
        let shaper = Arc::new(Shaper::new());
        let tid = mk_torrent_id(7);
        shaper.register_torrent_passthrough(tid);
        shaper.register_peer(PeerSlot(10), tid, DuplexBuckets::passthrough());
        shaper.register_peer(PeerSlot(11), tid, DuplexBuckets::passthrough());
        assert_eq!(shaper.peers.lock().unwrap().len(), 2);
        shaper.drop_torrent(tid);
        assert_eq!(shaper.peers.lock().unwrap().len(), 0);
        assert_eq!(shaper.torrents.lock().unwrap().len(), 0);
    }
}
