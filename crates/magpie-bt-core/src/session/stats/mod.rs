//! Per-peer + per-torrent stats (ADR-0014).
//!
//! Per-peer `AtomicU64 uploaded` + `AtomicU64 downloaded`, one atomic-add
//! per block, served as the single source of truth to three readers:
//!
//! 1. Choker EWMA (reads live atomics, **not** the alert stream — plan
//!    invariant #5 so choker decisions don't lag 1 s behind the 1 Hz
//!    emitter).
//! 2. Shaper demand signal (also live atomics).
//! 3. 1 Hz `StatsUpdate` alert emitter.
//!
//! ## Snapshot ordering (plan invariant #2)
//!
//! Per-torrent cumulative = sum(live peers) + `disconnected_sum`. On peer
//! disconnect, the peer task must:
//!
//! 1. `disconnected_sum.fetch_add(peer.uploaded.load(Acquire), Release)`
//! 2. (same for `downloaded`)
//! 3. *Then* signal the torrent actor to remove the peer from its registry.
//!
//! Reversing the order (remove then add) would drop the peer's counters
//! between the two steps: a snapshot taken mid-transition would miss them.
//! [`PerTorrentStats::retire_peer`] encapsulates the correct ordering.

pub mod sink;

pub use sink::{FileStatsSink, StatsSink, StatsSinkError};

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-peer counters. Owned by [`PeerStats`]; typically created at peer
/// registration and consumed at disconnect via
/// [`PerTorrentStats::retire_peer`].
#[derive(Debug, Default)]
pub struct PeerStats {
    /// Bytes sent to this peer (monotonic).
    pub uploaded: AtomicU64,
    /// Bytes received from this peer (monotonic).
    pub downloaded: AtomicU64,
}

impl PeerStats {
    /// Fresh zero counters.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            uploaded: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
        }
    }

    /// Account `bytes` uploaded to this peer.
    pub fn add_uploaded(&self, bytes: u64) {
        self.uploaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Account `bytes` downloaded from this peer.
    pub fn add_downloaded(&self, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Cheap snapshot of (uploaded, downloaded).
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.uploaded.load(Ordering::Acquire),
            self.downloaded.load(Ordering::Acquire),
        )
    }
}

/// Per-torrent counters accumulated from disconnected peers.
#[derive(Debug, Default)]
pub struct PerTorrentStats {
    /// Cumulative upload bytes from peers that have disconnected. Live
    /// peers contribute via their own [`PeerStats`].
    pub disconnected_up: AtomicU64,
    /// Cumulative download bytes from disconnected peers.
    pub disconnected_down: AtomicU64,
}

impl PerTorrentStats {
    /// Fresh torrent counters.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            disconnected_up: AtomicU64::new(0),
            disconnected_down: AtomicU64::new(0),
        }
    }

    /// Roll a peer's counters into the disconnected sum **before** the
    /// peer's [`PeerStats`] is dropped from the torrent's live set. Plan
    /// invariant #2: the `Release` on the `fetch_add` synchronises with the
    /// `Acquire` in [`Self::snapshot`], guaranteeing a snapshot that does
    /// not see the peer in the live set DOES see its contribution here.
    pub fn retire_peer(&self, peer: &PeerStats) {
        let (up, down) = peer.snapshot();
        self.disconnected_up.fetch_add(up, Ordering::Release);
        self.disconnected_down.fetch_add(down, Ordering::Release);
    }

    /// Cumulative (uploaded, downloaded) across the given live peers plus
    /// the disconnected-sum. Caller holds the torrent-actor lock that
    /// defines "live" so the read is consistent.
    #[must_use]
    pub fn snapshot<'a>(&self, live_peers: impl IntoIterator<Item = &'a PeerStats>) -> (u64, u64) {
        let mut up = self.disconnected_up.load(Ordering::Acquire);
        let mut down = self.disconnected_down.load(Ordering::Acquire);
        for p in live_peers {
            let (u, d) = p.snapshot();
            up = up.saturating_add(u);
            down = down.saturating_add(d);
        }
        (up, down)
    }
}

/// Snapshot of torrent-level stats for the emitter + sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Info-hash (identifies the torrent inside a session-global sink).
    pub info_hash: [u8; 20],
    /// Cumulative uploaded bytes (live + disconnected).
    pub uploaded: u64,
    /// Cumulative downloaded bytes.
    pub downloaded: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_sums_live_and_disconnected() {
        let t = PerTorrentStats::new();
        let p1 = PeerStats::new();
        let p2 = PeerStats::new();
        p1.add_uploaded(100);
        p1.add_downloaded(200);
        p2.add_uploaded(50);
        p2.add_downloaded(25);
        let (up, down) = t.snapshot([&p1, &p2]);
        assert_eq!(up, 150);
        assert_eq!(down, 225);
    }

    #[test]
    fn retire_peer_preserves_counters() {
        let t = PerTorrentStats::new();
        let p = PeerStats::new();
        p.add_uploaded(500);
        p.add_downloaded(700);
        // Plan invariant #2: retire BEFORE the peer is removed from the
        // live set. The simulated "removal" here is just dropping the
        // reference; the snapshot below uses an empty live-set.
        t.retire_peer(&p);
        // Snapshot now excludes p from the live set; disconnected sum
        // should hold its counters.
        let (up, down) = t.snapshot(std::iter::empty::<&PeerStats>());
        assert_eq!(up, 500);
        assert_eq!(down, 700);
    }

    #[test]
    fn snapshot_during_disconnect_never_loses_counters() {
        // Simulates the disconnect race: live-set + disconnected_sum both
        // observed. Plan invariant #2 ensures Acquire/Release pairing so
        // retire-then-remove yields a consistent snapshot.
        let t = PerTorrentStats::new();
        let p1 = PeerStats::new();
        let p2 = PeerStats::new();
        p1.add_uploaded(10);
        p2.add_uploaded(20);
        // Before retirement: sum = 30, disc = 0.
        let (up, _) = t.snapshot([&p1, &p2]);
        assert_eq!(up, 30);
        // Retire p2 but keep p1 in live-set.
        t.retire_peer(&p2);
        // After: live-set {p1} + disc = 30.
        let (up, _) = t.snapshot([&p1]);
        assert_eq!(up, 30);
    }
}
