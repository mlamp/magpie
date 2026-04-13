#![no_main]
//! Fuzz target: metainfo parse must not panic on any input.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // TODO(M0): call magpie_bt_metainfo::parse(data) when the parser lands.
    let _ = data.len();
});
