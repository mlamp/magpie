//! Gate #4: peer-ID builder produces valid 20-byte `-CCVVVV-<12-byte-suffix>`
//! IDs, and two consecutive calls differ in the suffix.
#![allow(missing_docs)]

use magpie_bt_core::peer_id::{PEER_ID_LEN, PeerIdBuilder};

#[test]
fn layout_is_exactly_20_bytes() {
    let id = PeerIdBuilder::magpie(*b"0001").build();
    assert_eq!(id.len(), PEER_ID_LEN);
    assert_eq!(&id[..8], b"-Mg0001-");
}

#[test]
fn consecutive_calls_differ_in_suffix() {
    let b = PeerIdBuilder::magpie(*b"0001");
    let a = b.build();
    let c = b.build();
    assert_eq!(&a[..8], &c[..8]);
    assert_ne!(a[8..], c[8..]);
}
