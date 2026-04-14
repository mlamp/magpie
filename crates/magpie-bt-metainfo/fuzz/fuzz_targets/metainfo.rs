#![no_main]
//! Fuzz target: metainfo parse must not panic on any input. If parsing
//! succeeds, the reported info-hash must match a fresh hash of `info_bytes`.
use libfuzzer_sys::fuzz_target;
use magpie_bt_metainfo::{InfoHash, parse, sha1, sha256};

fuzz_target!(|data: &[u8]| {
    if let Ok(meta) = parse(data) {
        match meta.info_hash {
            InfoHash::V1(h) => assert_eq!(h, sha1(meta.info_bytes)),
            InfoHash::V2(h) => assert_eq!(h, sha256(meta.info_bytes)),
            InfoHash::Hybrid { v1, v2 } => {
                assert_eq!(v1, sha1(meta.info_bytes));
                assert_eq!(v2, sha256(meta.info_bytes));
            }
        }
    }
});
