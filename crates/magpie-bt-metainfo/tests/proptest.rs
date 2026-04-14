//! Property tests for the metainfo parser.
#![allow(missing_docs)]

mod common;

use magpie_bt_metainfo::{InfoHash, parse};
use proptest::prelude::*;
use sha1::Digest as _;

proptest! {
    #[test]
    fn v1_single_always_parses(
        name in "[a-z]{1,10}",
        length in 1u64..(1 << 24),
        log_piece in 14u32..20, // piece_length in [16 KiB, 512 KiB]
    ) {
        let piece = 1u64 << log_piece;
        let bytes = common::synth_v1_single(&name, length, piece);
        let meta = parse(&bytes).unwrap();
        prop_assert!(meta.info.is_v1_only());
        prop_assert_eq!(meta.info.name, name.as_bytes());
        prop_assert_eq!(meta.info.piece_length, piece);
        // info_hash is stable across re-parses.
        let meta2 = parse(&bytes).unwrap();
        prop_assert_eq!(meta.info_hash, meta2.info_hash);
    }

    #[test]
    fn info_hash_matches_sha1_of_info_bytes(
        name in "[a-z]{1,8}",
        length in 1u64..(1 << 20),
    ) {
        let bytes = common::synth_v1_single(&name, length, 32768);
        let meta = parse(&bytes).unwrap();
        let rehash: [u8; 20] = sha1::Sha1::digest(meta.info_bytes).into();
        match meta.info_hash {
            InfoHash::V1(h) => prop_assert_eq!(h, rehash),
            _ => prop_assert!(false, "expected V1"),
        }
    }

    #[test]
    fn parser_never_panics_on_arbitrary_bytes(raw in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse(&raw);
    }
}
