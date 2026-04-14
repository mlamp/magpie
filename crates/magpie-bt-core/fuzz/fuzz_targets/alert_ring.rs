#![no_main]
//! Fuzz target: arbitrary sequences of push/drain/set-mask must leave the
//! alert ring in a consistent state and never panic.
//!
//! Invariants checked on each step:
//! - queue `pending()` never exceeds capacity;
//! - when a batch is drained, every alert in it either came from a prior
//!   accepted push or is an `Alert::Dropped` sentinel;
//! - after drain, `pending()` == 0.
use libfuzzer_sys::fuzz_target;
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertErrorCode, AlertQueue};
use magpie_bt_core::{PeerSlot, TorrentId};

const TID: TorrentId = TorrentId::__test_new(1);

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects capacity in 1..=64.
    let capacity = usize::from(data[0] % 64) + 1;
    let q = AlertQueue::new(capacity);
    let mut expected_pushes = 0_u64;

    for &b in &data[1..] {
        match b % 10 {
            0 => {
                if q.push(Alert::PieceCompleted { torrent: TID, piece: u32::from(b) }) {
                    expected_pushes += 1;
                }
            }
            1 => {
                if q.push(Alert::PeerConnected { torrent: TID, peer: PeerSlot(u64::from(b)) }) {
                    expected_pushes += 1;
                }
            }
            2 => {
                if q.push(Alert::PeerDisconnected { torrent: TID, peer: PeerSlot(u64::from(b)) }) {
                    expected_pushes += 1;
                }
            }
            3 => {
                if q.push(Alert::StatsTick) {
                    expected_pushes += 1;
                }
            }
            4 => {
                if q.push(Alert::TrackerResponse { torrent: TID }) {
                    expected_pushes += 1;
                }
            }
            5 => {
                if q.push(Alert::Error { torrent: TID, code: AlertErrorCode::PeerProtocol }) {
                    expected_pushes += 1;
                }
            }
            6 => {
                // Drain and count.
                let batch = q.drain();
                assert_eq!(q.pending(), 0);
                // Non-sentinel count is bounded by capacity and must be ≤ expected pushes.
                let non_sentinel = batch
                    .iter()
                    .filter(|a| !matches!(a, Alert::Dropped { .. }))
                    .count();
                assert!(non_sentinel <= capacity);
                assert!(non_sentinel as u64 <= expected_pushes);
                expected_pushes = expected_pushes.saturating_sub(non_sentinel as u64);
            }
            7 => q.set_mask(AlertCategory::ALL),
            8 => q.set_mask(AlertCategory::PIECE),
            9 => q.set_mask(AlertCategory::NONE),
            _ => unreachable!(),
        }
        assert!(q.pending() <= capacity);
    }
});
