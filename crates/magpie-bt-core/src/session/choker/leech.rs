//! Leech-side choker: tit-for-tat.
//!
//! Ranks peers by their 20 s-EWMA download rate *to us*. Peers that have
//! sent us data recently win slots; peers that haven't (anti-snub window)
//! drop out. Classical BitTorrent reciprocity.

use std::time::Instant;

use super::{ChokerConfig, PeerView, UnchokeSet, Unchoker, pick_optimistic};

/// Leech-side choker. See [`super`] module docs.
#[derive(Debug, Default)]
pub struct LeechChoker {
    cfg: ChokerConfig,
}

impl LeechChoker {
    /// Construct with default [`ChokerConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            cfg: ChokerConfig::default(),
        }
    }

    /// Construct with a custom config.
    #[must_use]
    pub const fn with_config(cfg: ChokerConfig) -> Self {
        Self { cfg }
    }
}

impl Unchoker for LeechChoker {
    fn select(&self, peers: &[PeerView], now: Instant, rotation_counter: u64) -> UnchokeSet {
        // Step 1: filter eligibility. Leech cares about peers who have sent
        // us data recently (within anti_snub) regardless of their interest
        // in us — we unchoke to *receive*, not to serve.
        let anti_snub = self.cfg.anti_snub;
        let eligible: Vec<PeerView> = peers
            .iter()
            .copied()
            .filter(|p| {
                // New peers get an anti-snub grace (no "last activity" yet).
                p.is_new || now.duration_since(p.last_activity) <= anti_snub
            })
            .collect();
        // Step 2: rate-rank — top-N by down_rate_bps, tie-break on slot id.
        let mut ranked = eligible.clone();
        ranked.sort_unstable_by(|a, b| {
            b.down_rate_bps
                .cmp(&a.down_rate_bps)
                .then_with(|| a.slot.0.cmp(&b.slot.0))
        });
        let regular: Vec<_> = ranked
            .iter()
            .take(self.cfg.regular_slots)
            .map(|p| p.slot)
            .collect();
        // Step 3: optimistic draw from the rest.
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
        UnchokeSet {
            regular,
            optimistic,
        }
    }

    fn config(&self) -> &ChokerConfig {
        &self.cfg
    }
}

#[cfg(test)]
#[allow(clippy::unchecked_time_subtraction)]
mod tests {
    use super::*;
    use crate::session::messages::PeerSlot;
    use std::time::Duration;

    fn peer(slot: u64, down: u64, last_activity: Instant) -> PeerView {
        PeerView {
            slot: PeerSlot(slot),
            down_rate_bps: down,
            up_rate_bps: 0,
            is_interested: false,
            is_new: false,
            last_activity,
        }
    }

    #[test]
    fn picks_top_n_by_down_rate() {
        let ch = LeechChoker::new();
        let now = Instant::now();
        let peers = vec![
            peer(1, 100, now),
            peer(2, 500, now),
            peer(3, 200, now),
            peer(4, 300, now),
            peer(5, 50, now),
        ];
        let set = ch.select(&peers, now, 0);
        // Top 4 by down rate: 2(500), 4(300), 3(200), 1(100). Optimistic:
        // whatever is left, which is peer 5.
        assert_eq!(
            set.regular,
            vec![PeerSlot(2), PeerSlot(4), PeerSlot(3), PeerSlot(1)]
        );
        assert_eq!(set.optimistic, vec![PeerSlot(5)]);
    }

    #[test]
    fn snubbed_peers_excluded() {
        let ch = LeechChoker::new();
        let now = Instant::now();
        let old = now - Duration::from_secs(120);
        let peers = vec![
            peer(1, 1000, old), // snubbed
            peer(2, 200, now),
            peer(3, 300, now),
        ];
        let set = ch.select(&peers, now, 0);
        assert!(
            !set.regular.contains(&PeerSlot(1)),
            "snubbed peer must not be unchoked"
        );
        assert!(set.regular.contains(&PeerSlot(2)));
        assert!(set.regular.contains(&PeerSlot(3)));
    }

    #[test]
    fn tie_break_deterministic_by_slot_id() {
        let ch = LeechChoker::new();
        let now = Instant::now();
        let peers = vec![peer(9, 100, now), peer(5, 100, now), peer(7, 100, now)];
        let set = ch.select(&peers, now, 0);
        // Equal rates → sorted by slot id ascending.
        assert_eq!(set.regular, vec![PeerSlot(5), PeerSlot(7), PeerSlot(9)]);
    }
}
