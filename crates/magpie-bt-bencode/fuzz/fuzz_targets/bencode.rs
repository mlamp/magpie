#![no_main]
//! Fuzz target: bencode decode must not panic on any input.
//!
//! Real invariants (decode → encode round-trip, bounded allocations) land
//! with the decoder during M0. Stub body asserts no panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // TODO(M0): call magpie_bt_bencode::decode(data) when the decoder lands.
    // For now, touch the slice so the fuzzer sees coverage on this code path.
    let _ = data.len();
});
