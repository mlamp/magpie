#![allow(
    clippy::cast_precision_loss,
    clippy::items_after_statements,
    clippy::missing_panics_doc
)]
//! Piece picker — rarest-first with endgame mode.
//!
//! The picker tracks, for each piece, (a) whether we already have it and (b)
//! how many known peers advertise it. At each [`Picker::pick`] call it
//! returns the index of a piece we still need, favouring pieces that the
//! fewest peers advertise (classical rarest-first). When the fraction of
//! still-missing pieces drops below a configurable threshold, endgame mode
//! engages and the picker switches to yielding any missing piece regardless
//! of rarity.
//!
//! This is the M0 skeleton: it validates the shape of the API against
//! synthetic swarm bitfields (gate criterion #3) and will grow priorities,
//! affinity, and per-block tracking in later milestones.
//!
//! ## Complexity contract
//!
//! - `pick` and `pick_n` are O(`piece_count`) today. Comfortable up to ~10k
//!   pieces; a 1 M-piece torrent at high request rate will want an indexed
//!   heap (rasterbar / librqbit pattern). Tracked for M1+.
//! - `in_endgame` uses `f32` division. Exact up to ~2^24 pieces; precision
//!   loss beyond is cosmetic (threshold engages within one piece of intent).
//!
//! # Example
//! ```
//! use magpie_bt_core::picker::Picker;
//!
//! let mut p = Picker::new(6);
//! // Peer A advertises pieces [0,1,2], peer B advertises [1,2,3,4].
//! p.observe_peer_bitfield(&[true, true, true, false, false, false]);
//! p.observe_peer_bitfield(&[false, true, true, true, true, false]);
//! // Piece 0 is the rarest (only peer A has it) — the picker yields it first.
//! assert_eq!(p.pick(), Some(0));
//! p.mark_have(0);
//! ```

use std::collections::BinaryHeap;

/// Default endgame threshold.
///
/// Endgame engages when the fraction of missing pieces drops below this value.
/// 5% matches the ballpark used by rasterbar and librqbit (both tune around
/// the last dozen pieces on large torrents).
pub const DEFAULT_ENDGAME_THRESHOLD: f32 = 0.05;

/// Piece picker state.
#[derive(Debug, Clone)]
pub struct Picker {
    /// For each piece: how many known peers advertise it.
    availability: Vec<u32>,
    /// For each piece: have we fully downloaded it?
    have: Vec<bool>,
    /// Endgame threshold — fraction of remaining pieces below which endgame engages.
    endgame_threshold: f32,
    /// Cached count of missing pieces (pieces where `!have[i]`).
    missing: u32,
}

impl Picker {
    /// Creates a new picker for a torrent with `piece_count` pieces. Initially
    /// all pieces are missing and availability is zero.
    ///
    /// # Panics
    /// Panics if `piece_count` is zero.
    #[must_use]
    pub fn new(piece_count: u32) -> Self {
        assert!(piece_count > 0, "piece_count must be > 0");
        Self {
            availability: vec![0; piece_count as usize],
            have: vec![false; piece_count as usize],
            endgame_threshold: DEFAULT_ENDGAME_THRESHOLD,
            missing: piece_count,
        }
    }

    /// Overrides the default endgame threshold (fraction of pieces still
    /// missing that triggers endgame).
    #[must_use]
    pub const fn with_endgame_threshold(mut self, threshold: f32) -> Self {
        self.endgame_threshold = threshold;
        self
    }

    /// Returns the number of pieces tracked.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn piece_count(&self) -> u32 {
        // Piece count was validated to fit in u32 at construction (see `new`).
        self.have.len() as u32
    }

    /// Returns the number of pieces not yet marked as `have`.
    #[must_use]
    pub const fn missing_count(&self) -> u32 {
        self.missing
    }

    /// Returns `true` if we've already downloaded piece `index`.
    ///
    /// # Panics
    /// Panics if `index` is out of range.
    #[must_use]
    pub fn has_piece(&self, index: u32) -> bool {
        self.have[index as usize]
    }

    /// Returns the current availability (peer count) for piece `index`.
    ///
    /// # Panics
    /// Panics if `index` is out of range.
    #[must_use]
    pub fn availability(&self, index: u32) -> u32 {
        self.availability[index as usize]
    }

    /// Records that the local client now owns piece `index`. Idempotent.
    ///
    /// # Panics
    /// Panics if `index` is out of range.
    pub fn mark_have(&mut self, index: u32) {
        let slot = &mut self.have[index as usize];
        if !*slot {
            *slot = true;
            self.missing -= 1;
        }
    }

    /// Records that a new peer advertised the given bitfield (one bool per
    /// piece; `true` = peer has it). Panics if the length is wrong.
    ///
    /// The per-piece counter saturates at `u32::MAX`, matching
    /// [`Picker::forget_peer_bitfield`]'s saturating decrement — a malicious
    /// peer observer cannot wrap the counter by spamming the same piece.
    pub fn observe_peer_bitfield(&mut self, bits: &[bool]) {
        assert_eq!(bits.len(), self.have.len(), "bitfield length mismatch");
        for (i, present) in bits.iter().enumerate() {
            if *present {
                let slot = &mut self.availability[i];
                *slot = slot.saturating_add(1);
            }
        }
    }

    /// Records that a known peer dropped. All bits they previously advertised
    /// should be passed here so availability counters stay accurate.
    pub fn forget_peer_bitfield(&mut self, bits: &[bool]) {
        assert_eq!(bits.len(), self.have.len(), "bitfield length mismatch");
        for (i, present) in bits.iter().enumerate() {
            if *present {
                let slot = &mut self.availability[i];
                *slot = slot.saturating_sub(1);
            }
        }
    }

    /// Returns `true` if the picker should be in endgame mode given the
    /// current ratio of missing pieces.
    #[must_use]
    pub fn in_endgame(&self) -> bool {
        let total = self.have.len() as f32;
        let missing = self.missing as f32;
        // Endgame whenever the remainder is at or below the threshold fraction.
        missing > 0.0 && missing / total <= self.endgame_threshold
    }

    /// Picks the next piece to request.
    ///
    /// Returns `None` when every piece is already owned.
    ///
    /// Normal mode: returns the index of a missing piece with the **lowest**
    /// availability > 0, ties broken by lowest index. Pieces no peer
    /// advertises are skipped — we cannot fetch them yet.
    ///
    /// Endgame mode: returns any missing piece (lowest index), ignoring
    /// availability entirely so every peer that has any missing piece gets
    /// requested. This matches rasterbar's "request from everyone" endgame.
    #[must_use]
    pub fn pick(&self) -> Option<u32> {
        if self.missing == 0 {
            return None;
        }
        if self.in_endgame() {
            return self
                .have
                .iter()
                .position(|h| !h)
                .and_then(|p| u32::try_from(p).ok());
        }
        // Normal path: scan for the rarest missing piece with availability ≥ 1.
        let mut best: Option<(u32, u32)> = None; // (availability, index)
        for (idx, have) in self.have.iter().enumerate() {
            if *have {
                continue;
            }
            let avail = self.availability[idx];
            if avail == 0 {
                continue;
            }
            let idx_u32 = u32::try_from(idx).expect("piece count fits in u32");
            match best {
                None => best = Some((avail, idx_u32)),
                Some((cur_avail, _)) if avail < cur_avail => best = Some((avail, idx_u32)),
                _ => {}
            }
        }
        best.map(|(_, i)| i)
    }

    /// Yields an ordered iterator over the first `n` candidate pieces in
    /// normal mode (rarest first, ties by lowest index). Useful for the
    /// session to batch multiple requests per tick.
    ///
    /// In endgame mode, this is equivalent to iterating missing pieces in
    /// ascending order.
    #[must_use]
    pub fn pick_n(&self, n: usize) -> Vec<u32> {
        if self.in_endgame() {
            return self
                .have
                .iter()
                .enumerate()
                .filter_map(|(i, have)| if *have { None } else { u32::try_from(i).ok() })
                .take(n)
                .collect();
        }
        // Heap ordered by (availability asc, index asc). BinaryHeap is a max
        // heap, so we invert the key.
        #[derive(Eq, PartialEq)]
        struct Key {
            avail: u32,
            idx: u32,
        }
        impl Ord for Key {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // Reverse so the smallest availability pops first.
                other
                    .avail
                    .cmp(&self.avail)
                    .then_with(|| other.idx.cmp(&self.idx))
            }
        }
        impl PartialOrd for Key {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        let mut heap = BinaryHeap::with_capacity(self.missing as usize);
        for (idx, have) in self.have.iter().enumerate() {
            if *have {
                continue;
            }
            let avail = self.availability[idx];
            if avail == 0 {
                continue;
            }
            heap.push(Key {
                avail,
                idx: u32::try_from(idx).expect("piece count fits in u32"),
            });
        }
        let mut out = Vec::with_capacity(n.min(heap.len()));
        for _ in 0..n {
            match heap.pop() {
                Some(k) => out.push(k.idx),
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_rarest_piece() {
        let mut p = Picker::new(6);
        p.observe_peer_bitfield(&[true, true, true, false, false, false]);
        p.observe_peer_bitfield(&[false, true, true, true, true, false]);
        // Availability: [1, 2, 2, 1, 1, 0]. Missing everything. Rarest
        // non-zero: pieces 0, 3, 4 all at 1. Lowest index: 0.
        assert_eq!(p.pick(), Some(0));
    }

    #[test]
    fn skips_unavailable_pieces_in_normal_mode() {
        let mut p = Picker::new(4);
        // Only piece 2 is advertised by any peer.
        p.observe_peer_bitfield(&[false, false, true, false]);
        assert_eq!(p.pick(), Some(2));
    }

    #[test]
    fn returns_none_when_complete() {
        let mut p = Picker::new(3);
        p.observe_peer_bitfield(&[true, true, true]);
        for i in 0..3 {
            p.mark_have(i);
        }
        assert_eq!(p.pick(), None);
    }

    #[test]
    fn endgame_engages_below_threshold() {
        let mut p = Picker::new(20).with_endgame_threshold(0.1); // 10%
        p.observe_peer_bitfield(&[true; 20]);
        for i in 0..18 {
            p.mark_have(i);
        }
        // 2/20 = 10% — in_endgame returns true at exactly the threshold.
        assert!(p.in_endgame());
    }

    #[test]
    fn endgame_returns_any_missing_piece() {
        let mut p = Picker::new(5).with_endgame_threshold(1.0); // always endgame
        // No peer advertises piece 3, but endgame ignores availability.
        p.observe_peer_bitfield(&[true, true, true, false, true]);
        p.mark_have(0);
        p.mark_have(1);
        p.mark_have(2);
        p.mark_have(4);
        assert!(p.in_endgame());
        assert_eq!(p.pick(), Some(3));
    }

    #[test]
    fn uniform_distribution_prefers_lowest_index() {
        let mut p = Picker::new(10);
        // All peers advertise all pieces.
        for _ in 0..5 {
            p.observe_peer_bitfield(&[true; 10]);
        }
        // Tie — pick piece 0 first.
        assert_eq!(p.pick(), Some(0));
        let batch = p.pick_n(3);
        assert_eq!(batch, vec![0, 1, 2]);
    }

    #[test]
    fn skewed_distribution_prefers_rare() {
        let mut p = Picker::new(4);
        // Piece 0: 3 peers. Piece 1: 1 peer. Piece 2: 2 peers. Piece 3: 0 peers.
        p.observe_peer_bitfield(&[true, false, true, false]);
        p.observe_peer_bitfield(&[true, false, true, false]);
        p.observe_peer_bitfield(&[true, true, false, false]);
        // Rarest with availability > 0 is piece 1.
        assert_eq!(p.pick(), Some(1));
        // pick_n keeps sorting rarity-first: 1, 2, 0 (skipping piece 3 which is 0-avail).
        let batch = p.pick_n(10);
        assert_eq!(batch, vec![1, 2, 0]);
    }

    #[test]
    fn near_complete_switches_to_endgame() {
        let mut p = Picker::new(100).with_endgame_threshold(0.05); // default
        p.observe_peer_bitfield(&[true; 100]);
        for i in 0..96 {
            p.mark_have(i);
        }
        // 4/100 = 4% < 5% → endgame.
        assert!(p.in_endgame());
    }

    #[test]
    fn forget_peer_decrements_availability() {
        let mut p = Picker::new(3);
        p.observe_peer_bitfield(&[true, true, true]);
        p.observe_peer_bitfield(&[true, false, true]);
        assert_eq!(p.availability(0), 2);
        p.forget_peer_bitfield(&[true, false, true]);
        assert_eq!(p.availability(0), 1);
        assert_eq!(p.availability(1), 1);
        assert_eq!(p.availability(2), 1);
    }

    #[test]
    fn mark_have_is_idempotent() {
        let mut p = Picker::new(3);
        p.observe_peer_bitfield(&[true, true, true]);
        let before = p.missing_count();
        p.mark_have(1);
        p.mark_have(1);
        assert_eq!(p.missing_count(), before - 1);
    }
}
