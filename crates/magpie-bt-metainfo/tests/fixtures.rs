//! Gate #1 verification: parse synthetic v1, v2, and hybrid torrents and
//! confirm each produces the expected `InfoHash` variant.
#![allow(missing_docs)]

mod common;

use magpie_bt_metainfo::{FileListV1, FileTreeNode, InfoHash, parse};
use sha1::Digest as _;

#[test]
fn parses_v1_single_file() {
    let bytes = common::synth_v1_single("hello.bin", 65536, 32768);
    let meta = parse(&bytes).unwrap();
    assert!(meta.info.is_v1_only(), "expected v1-only");
    assert_eq!(meta.info.name, b"hello.bin");
    assert_eq!(meta.info.piece_length, 32768);
    let v1 = meta.info.v1.as_ref().unwrap();
    assert_eq!(v1.pieces.len(), 40); // 2 pieces * 20 bytes
    match &v1.files {
        FileListV1::Single { length } => assert_eq!(*length, 65536),
        FileListV1::Multi { .. } => panic!("expected single-file"),
    }

    // Info-hash must be V1 and must match SHA-1 of the info span.
    let expected = common::expected_v1_info_hash(&bytes);
    match meta.info_hash {
        InfoHash::V1(h) => assert_eq!(h, expected),
        other => panic!("expected V1, got {other:?}"),
    }
}

#[test]
fn parses_v1_multi_file() {
    let files: &[(u64, &[&str])] = &[
        (1024, &["a", "one.txt"]),
        (2048, &["a", "two.txt"]),
        (4096, &["b", "three.bin"]),
    ];
    let bytes = common::synth_v1_multi("pack", files, 16384);
    let meta = parse(&bytes).unwrap();
    assert!(meta.info.is_v1_only());
    let v1 = meta.info.v1.as_ref().unwrap();
    match &v1.files {
        FileListV1::Multi { files } => {
            assert_eq!(files.len(), 3);
            assert_eq!(files[0].length, 1024);
            assert_eq!(files[0].path, vec![&b"a"[..], &b"one.txt"[..]]);
        }
        FileListV1::Single { .. } => panic!("expected multi-file"),
    }
}

#[test]
fn parses_v2_single_file() {
    let bytes = common::synth_v2_single("hello.v2", 200_000, 16384);
    let meta = parse(&bytes).unwrap();
    assert!(
        meta.info.is_v2_only(),
        "expected v2-only, got v1={:?} v2={:?}",
        meta.info.v1.is_some(),
        meta.info.v2.is_some()
    );
    assert_eq!(meta.info.name, b"hello.v2");

    let v2 = meta.info.v2.as_ref().unwrap();
    assert_eq!(v2.meta_version, 2);
    // Root dir must have one entry named `hello.v2` pointing at a file leaf.
    match &v2.file_tree {
        FileTreeNode::Dir(children) => {
            let leaf = children.get(&b"hello.v2"[..]).expect("leaf missing");
            match leaf {
                FileTreeNode::File {
                    length,
                    pieces_root,
                } => {
                    assert_eq!(*length, 200_000);
                    assert_eq!(*pieces_root, Some([0x11_u8; 32]));
                }
                FileTreeNode::Dir(_) => panic!("expected file leaf"),
            }
        }
        FileTreeNode::File { .. } => panic!("root must be a directory"),
    }

    let expected = common::expected_v2_info_hash(&bytes);
    match meta.info_hash {
        InfoHash::V2(h) => assert_eq!(h, expected),
        other => panic!("expected V2, got {other:?}"),
    }
}

#[test]
fn parses_hybrid() {
    let bytes = common::synth_hybrid_single("hybrid.bin", 65536, 32768);
    let meta = parse(&bytes).unwrap();
    assert!(meta.info.is_hybrid());

    let expected_v1 = common::expected_v1_info_hash(&bytes);
    let expected_v2 = common::expected_v2_info_hash(&bytes);
    match meta.info_hash {
        InfoHash::Hybrid { v1, v2 } => {
            assert_eq!(v1, expected_v1);
            assert_eq!(v2, expected_v2);
        }
        other => panic!("expected Hybrid, got {other:?}"),
    }
}

#[test]
fn info_bytes_matches_hash_input() {
    let bytes = common::synth_v1_single("x.bin", 1024, 32768);
    let meta = parse(&bytes).unwrap();
    // info_bytes must be a slice of the original torrent, unchanged.
    assert!(
        bytes
            .windows(meta.info_bytes.len())
            .any(|w| w == meta.info_bytes)
    );
    // Re-hash info_bytes; must match the reported V1.
    let rehash: [u8; 20] = sha1::Sha1::digest(meta.info_bytes).into();
    assert_eq!(Some(&rehash), meta.info_hash.v1());
}
