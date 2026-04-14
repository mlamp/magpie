//! Choker (ADR-0012).
//!
//! Two implementations of the [`Unchoker`] trait:
//!
//! - [`leech::LeechChoker`] — tit-for-tat; rank peers by 20 s-EWMA
//!   download rate *from* the peer. Rewards peers that give us data.
//! - [`seed::SeedChoker`] — rasterbar `fastest_upload` model; rank peers by
//!   20 s-EWMA upload rate *to* the peer. Explicitly **not** the broken
//!   round-robin-by-bytes algorithm that lets slow peers monopolise slots.
//!
//! Both serve 4 regular slots + 1 optimistic slot (configurable). Regular
//! slots rotate every 10 s, optimistic every 30 s. New peers get a 3× weight
//! on the optimistic draw so they get a chance to prove themselves.
//! Leech-side applies a 60 s anti-snub: a peer we've unchoked but hasn't
//! sent us anything is dropped from the regular set.
//!
//! The choker is a **pure function** of the peer view plus a rotation
//! counter — no actor wiring, no async. The torrent actor calls
//! [`Unchoker::select`] on rotation ticks and diffs the new set against the
//! old, issuing `SessionToPeer::{Choke, Unchoke}` accordingly.

use std::time::{Duration, Instant};

use crate::session::messages::PeerSlot;

pub mod leech;
pub mod seed;

pub use leech::LeechChoker;
pub use seed::SeedChoker;

/// Default regular (rate-ranked) unchoke slots.
pub const DEFAULT_REGULAR_SLOTS: usize = 4;

/// Default optimistic unchoke slots.
pub const DEFAULT_OPTIMISTIC_SLOTS: usize = 1;

/// Default leech-side anti-snub window: a peer we've unchoked but who hasn't
/// delivered a block in this long is dropped from consideration.
pub const DEFAULT_ANTI_SNUB: Duration = Duration::from_secs(60);

/// Default weight multiplier for new (recently-connected) peers in the
/// optimistic draw.
pub const DEFAULT_NEW_PEER_WEIGHT: u32 = 3;

/// Default regular rotation cadence.
pub const DEFAULT_REGULAR_ROTATION: Duration = Duration::from_secs(10);

/// Default optimistic rotation cadence.
pub const DEFAULT_OPTIMISTIC_ROTATION: Duration = Duration::from_secs(30);

/// Choker parameters.
#[derive(Debug, Clone, Copy)]
pub struct ChokerConfig {
    /// Rate-ranked unchoke slot count.
    pub regular_slots: usize,
    /// Optimistic unchoke slot count.
    pub optimistic_slots: usize,
    /// Anti-snub window (leech-side only).
    pub anti_snub: Duration,
    /// Weight multiplier for new peers in the optimistic draw.
    pub new_peer_weight: u32,
}

impl Default for ChokerConfig {
    fn default() -> Self {
        Self {
            regular_slots: DEFAULT_REGULAR_SLOTS,
            optimistic_slots: DEFAULT_OPTIMISTIC_SLOTS,
            anti_snub: DEFAULT_ANTI_SNUB,
            new_peer_weight: DEFAULT_NEW_PEER_WEIGHT,
        }
    }
}

/// A snapshot of a peer from the torrent actor's perspective. Fed to
/// [`Unchoker::select`].
#[derive(Debug, Clone, Copy)]
pub struct PeerView {
    /// Actor-issued slot id.
    pub slot: PeerSlot,
    /// 20 s-EWMA download rate *from* this peer (bytes/sec). Used by
    /// [`LeechChoker`].
    pub down_rate_bps: u64,
    /// 20 s-EWMA upload rate *to* this peer (bytes/sec). Used by
    /// [`SeedChoker`].
    pub up_rate_bps: u64,
    /// Whether the peer is interested in downloading from us (seed-side
    /// prerequisite).
    pub is_interested: bool,
    /// Whether the peer is a "new" peer for optimistic weighting purposes
    /// (typically: connected within the last one optimistic rotation).
    pub is_new: bool,
    /// Most recent useful activity (leech: block delivered; seed: request
    /// received). Leech choker applies `anti_snub` against this.
    pub last_activity: Instant,
}

/// Output of [`Unchoker::select`] — the slot sets the torrent actor should
/// diff against the current unchoke state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnchokeSet {
    /// Rate-ranked unchoke set.
    pub regular: Vec<PeerSlot>,
    /// Optimistic unchoke set (size ≤ `optimistic_slots`).
    pub optimistic: Vec<PeerSlot>,
}

impl UnchokeSet {
    /// All peers to leave unchoked in this rotation.
    #[must_use]
    pub fn all(&self) -> Vec<PeerSlot> {
        let mut out = self.regular.clone();
        out.extend_from_slice(&self.optimistic);
        out
    }

    /// Iterator over all slots (regular + optimistic) without allocating.
    pub fn iter(&self) -> impl Iterator<Item = PeerSlot> + '_ {
        self.regular
            .iter()
            .copied()
            .chain(self.optimistic.iter().copied())
    }
}

/// Unchoker strategy.
pub trait Unchoker {
    /// Compute the next unchoke set from the peer view. `rotation_counter`
    /// should increase monotonically across calls; it seeds the deterministic
    /// optimistic choice so tests get stable output.
    fn select(&self, peers: &[PeerView], now: Instant, rotation_counter: u64) -> UnchokeSet;

    /// Configuration (for tests + diagnostics).
    fn config(&self) -> &ChokerConfig;
}

/// Pick `n` peers with weighted pseudo-random selection, weighting "new"
/// peers higher. Deterministic given `rotation_counter` to make tests stable.
///
/// Used internally by both [`LeechChoker`] and [`SeedChoker`] for the
/// optimistic slots.
fn pick_optimistic(
    eligible: &[PeerView],
    n: usize,
    new_peer_weight: u32,
    rotation_counter: u64,
) -> Vec<PeerSlot> {
    if eligible.is_empty() || n == 0 {
        return Vec::new();
    }
    // Build weighted slot list. O(N) in peers but N is small (≤ a few hundred).
    let mut picks = Vec::with_capacity(n);
    let mut remaining: Vec<&PeerView> = eligible.iter().collect();
    let mut counter = rotation_counter;
    for _ in 0..n.min(eligible.len()) {
        let total_weight: u64 = remaining
            .iter()
            .map(|p| {
                if p.is_new {
                    u64::from(new_peer_weight)
                } else {
                    1
                }
            })
            .sum();
        if total_weight == 0 {
            break;
        }
        // Deterministic seed: splitmix64 step.
        counter = counter.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = counter;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let target = z % total_weight;
        let mut acc = 0u64;
        let mut chosen_idx = 0usize;
        for (i, p) in remaining.iter().enumerate() {
            let w = if p.is_new {
                u64::from(new_peer_weight)
            } else {
                1
            };
            acc += w;
            if acc > target {
                chosen_idx = i;
                break;
            }
        }
        let chosen = remaining.remove(chosen_idx);
        picks.push(chosen.slot);
    }
    picks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkpeer(slot: u64, down: u64, up: u64, interested: bool, is_new: bool) -> PeerView {
        PeerView {
            slot: PeerSlot(slot),
            down_rate_bps: down,
            up_rate_bps: up,
            is_interested: interested,
            is_new,
            last_activity: Instant::now(),
        }
    }

    #[test]
    fn pick_optimistic_returns_nothing_when_no_eligible() {
        let result = pick_optimistic(&[], 1, 3, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn pick_optimistic_picks_at_most_n() {
        let peers = vec![
            mkpeer(1, 0, 0, true, false),
            mkpeer(2, 0, 0, true, false),
            mkpeer(3, 0, 0, true, false),
        ];
        let result = pick_optimistic(&peers, 2, 3, 42);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn pick_optimistic_is_deterministic() {
        let peers = vec![
            mkpeer(1, 0, 0, true, false),
            mkpeer(2, 0, 0, true, false),
            mkpeer(3, 0, 0, true, false),
        ];
        let a = pick_optimistic(&peers, 2, 3, 42);
        let b = pick_optimistic(&peers, 2, 3, 42);
        assert_eq!(a, b);
    }
}
