//! Seed-side choker: rasterbar `fastest_upload`.
//!
//! Ranks peers by 20 s-EWMA upload rate *to* them — the fastest consumers
//! win slots. Explicitly **not** round-robin-by-bytes, which is the broken
//! original rasterbar algorithm that let slow peers monopolise slots.
//! Interested peers only (we don't serve peers who haven't signalled
//! interest). No anti-snub on the seed side: if we're choking them, they
//! naturally can't demonstrate recent activity either way.

use std::time::Instant;

use super::{ChokerConfig, PeerView, UnchokeSet, Unchoker, pick_optimistic};

/// Seed-side choker. See [`super`] module docs.
#[derive(Debug, Default)]
pub struct SeedChoker {
    cfg: ChokerConfig,
}

impl SeedChoker {
    /// Construct with default [`ChokerConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self { cfg: ChokerConfig::default() }
    }

    /// Construct with a custom config.
    #[must_use]
    pub const fn with_config(cfg: ChokerConfig) -> Self {
        Self { cfg }
    }
}

impl Unchoker for SeedChoker {
    fn select(&self, peers: &[PeerView], _now: Instant, rotation_counter: u64) -> UnchokeSet {
        let eligible: Vec<PeerView> = peers.iter().copied().filter(|p| p.is_interested).collect();
        let mut ranked = eligible.clone();
        ranked.sort_unstable_by(|a, b| {
            b.up_rate_bps
                .cmp(&a.up_rate_bps)
                .then_with(|| a.slot.0.cmp(&b.slot.0))
        });
        let regular: Vec<_> = ranked
            .iter()
            .take(self.cfg.regular_slots)
            .map(|p| p.slot)
            .collect();
        let rest: Vec<PeerView> = eligible
            .into_iter()
            .filter(|p| !regular.contains(&p.slot))
            .collect();
        let optimistic = pick_optimistic(
            &rest,
            self.cfg.optimistic_slots,
            self.cfg.new_peer_weight,
            rotation_counter,
        );
        UnchokeSet { regular, optimistic }
    }

    fn config(&self) -> &ChokerConfig {
        &self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::messages::PeerSlot;

    fn peer(slot: u64, up: u64, interested: bool) -> PeerView {
        PeerView {
            slot: PeerSlot(slot),
            down_rate_bps: 0,
            up_rate_bps: up,
            is_interested: interested,
            is_new: false,
            last_activity: Instant::now(),
        }
    }

    #[test]
    fn picks_fastest_consumers_first() {
        let ch = SeedChoker::new();
        let peers = vec![
            peer(1, 100, true),
            peer(2, 500, true),
            peer(3, 200, true),
            peer(4, 300, true),
        ];
        let set = ch.select(&peers, Instant::now(), 0);
        assert_eq!(set.regular[0], PeerSlot(2)); // fastest at top
        assert_eq!(set.regular[1], PeerSlot(4));
        assert_eq!(set.regular[2], PeerSlot(3));
        assert_eq!(set.regular[3], PeerSlot(1));
    }

    #[test]
    fn excludes_uninterested_peers() {
        let ch = SeedChoker::new();
        let peers = vec![
            peer(1, 1000, false), // not interested — must not be unchoked
            peer(2, 100, true),
        ];
        let set = ch.select(&peers, Instant::now(), 0);
        assert!(!set.regular.contains(&PeerSlot(1)));
        assert!(set.regular.contains(&PeerSlot(2)));
        assert!(!set.optimistic.contains(&PeerSlot(1)));
    }
}
