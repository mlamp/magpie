//! Gate #3: piece picker sanity on synthetic swarm bitfields.
#![allow(missing_docs)]

use magpie_bt_core::picker::Picker;

fn bitfield(piece_count: u32, predicate: impl Fn(u32) -> bool) -> Vec<bool> {
    (0..piece_count).map(&predicate).collect()
}

/// Uniform distribution: every peer advertises every piece.
/// Rarest-first should degenerate to lowest-index ordering.
#[test]
fn uniform_swarm() {
    let n = 32;
    let mut p = Picker::new(n);
    for _ in 0..8 {
        p.observe_peer_bitfield(&vec![true; n as usize]);
    }
    let picks = p.pick_n(n as usize);
    assert_eq!(picks.len(), n as usize);
    assert!(picks.iter().zip(0_u32..).all(|(got, want)| *got == want));
}

/// Skewed distribution: piece 0 is advertised by all peers, piece `i` by only
/// one peer for `i > 0`. Rarest-first should pick piece 1 first, then any
/// other singly-advertised piece, then piece 0 last.
#[test]
fn skewed_swarm() {
    let n = 8;
    let mut p = Picker::new(n);
    // 3 peers have piece 0 + one unique other piece each.
    p.observe_peer_bitfield(&bitfield(n, |i| i == 0 || i == 1));
    p.observe_peer_bitfield(&bitfield(n, |i| i == 0 || i == 2));
    p.observe_peer_bitfield(&bitfield(n, |i| i == 0 || i == 3));
    // Pieces 4..8 are advertised by no one, so rarest-first must skip them in normal mode.
    let picks = p.pick_n(4);
    assert_eq!(picks.len(), 4);
    // First three picks are the singly-advertised pieces (1, 2, 3 — ties broken by index).
    assert_eq!(&picks[..3], &[1, 2, 3]);
    // Fourth is piece 0 (advertised by three peers).
    assert_eq!(picks[3], 0);
}

/// Near-complete: all but 3 pieces already owned. Endgame engages at 5%
/// threshold when 3/100 = 3% remain.
#[test]
fn near_complete_endgame() {
    let n = 100;
    let mut p = Picker::new(n);
    p.observe_peer_bitfield(&vec![true; n as usize]);
    for i in 0..97 {
        p.mark_have(i);
    }
    assert!(p.in_endgame());
    // Endgame returns any missing piece; lowest-index first = 97.
    let first = p.pick().unwrap();
    assert_eq!(first, 97);
    let batch = p.pick_n(10);
    // Only 3 missing.
    assert_eq!(batch, vec![97, 98, 99]);
}

/// Progress: `pick` → `mark_have` → next pick → etc. should drain the swarm
/// deterministically and never panic.
#[test]
fn progress_without_panic() {
    let n = 16;
    let mut p = Picker::new(n);
    p.observe_peer_bitfield(&vec![true; n as usize]);
    let mut count = 0_u32;
    while let Some(idx) = p.pick() {
        p.mark_have(idx);
        count += 1;
        assert!(count <= n, "loop didn't terminate");
    }
    assert_eq!(count, n);
    assert_eq!(p.missing_count(), 0);
}
