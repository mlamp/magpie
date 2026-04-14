#![no_main]
//! Fuzz target: `bencode::decode` must never panic and must never allocate
//! more than the input size implies. If decoding succeeds, re-encoding the
//! value must produce bytes that decode back to the same tree.
use libfuzzer_sys::fuzz_target;
use magpie_bt_bencode::{decode, encode};

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = decode(data) {
        let reencoded = encode(&value);
        let reparsed = decode(&reencoded).expect("canonical re-encode must decode");
        assert_eq!(reparsed.into_owned(), value.into_owned());
    }
});
