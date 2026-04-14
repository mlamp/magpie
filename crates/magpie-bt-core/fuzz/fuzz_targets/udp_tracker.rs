#![no_main]
//! Fuzz target: BEP 15 UDP tracker response decoders must never panic on
//! arbitrary input; any well-formed response decodes consistently; any
//! malformed one returns `TrackerError`, never a panic / OOB slice / integer
//! overflow.
//!
//! Input shape: first byte selects which decoder, next 4 bytes are the
//! expected transaction-id, remaining bytes are the response payload.
//!
//! Covers: `decode_connect`, `decode_announce` (BEP 15 §3.2/3.3). The
//! `action=ERROR` path, short-length rejection, txid-mismatch rejection, and
//! trailing-byte-count rounding in the announce peer list are all reachable
//! under fuzzing.
use libfuzzer_sys::fuzz_target;
use magpie_bt_core::tracker::udp::{decode_announce, decode_connect};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let selector = data[0];
    let expected_txid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let payload = &data[5..];

    match selector % 2 {
        0 => {
            // decode_connect must terminate without panic on any slice.
            let _ = decode_connect(payload, expected_txid);
        }
        _ => {
            // decode_announce must terminate without panic on any slice;
            // when it returns Ok, the reported peer count must fit the
            // advertised body length.
            if let Ok(resp) = decode_announce(payload, expected_txid) {
                // Sanity: announce response header is 20 bytes, peers are
                // 6 bytes each — so peers.len() * 6 must be representable
                // and must not exceed what the payload can carry.
                let peer_bytes = resp.peers.len().saturating_mul(6);
                assert!(peer_bytes <= payload.len());
            }
        }
    }
});
