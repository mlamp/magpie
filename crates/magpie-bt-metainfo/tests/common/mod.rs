//! Shared helpers: synthetic torrent builders used across integration tests,
//! property tests, and fuzz seed generation.
#![allow(dead_code, unreachable_pub, clippy::missing_panics_doc)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::use_self
)]

use sha1::{Digest as _, Sha1};
use sha2::Sha256;

/// Build a minimal v1 single-file torrent: one piece of zeros.
#[must_use]
pub fn synth_v1_single(name: &str, length: u64, piece_length: u64) -> Vec<u8> {
    let num_pieces = length.div_ceil(piece_length).max(1) as usize;
    let pieces_blob = vec![0_u8; num_pieces * 20];
    encode_root(&[(
        b"info",
        Enc::Dict(vec![
            (b"length", Enc::Int(length as i64)),
            (b"name", Enc::Bytes(name.as_bytes())),
            (b"piece length", Enc::Int(piece_length as i64)),
            (b"pieces", Enc::Bytes(&pieces_blob)),
        ]),
    )])
}

/// Build a minimal v1 multi-file torrent.
#[must_use]
pub fn synth_v1_multi(name: &str, files: &[(u64, &[&str])], piece_length: u64) -> Vec<u8> {
    let total: u64 = files.iter().map(|(l, _)| *l).sum();
    let num_pieces = total.div_ceil(piece_length).max(1) as usize;
    let pieces_blob = vec![0_u8; num_pieces * 20];
    let files_enc: Vec<Enc> = files
        .iter()
        .map(|(length, path)| {
            let path_enc: Vec<Enc> = path.iter().map(|c| Enc::Bytes(c.as_bytes())).collect();
            Enc::Dict(vec![
                (b"length", Enc::Int(*length as i64)),
                (b"path", Enc::List(path_enc)),
            ])
        })
        .collect();
    encode_root(&[(
        b"info",
        Enc::Dict(vec![
            (b"files", Enc::List(files_enc)),
            (b"name", Enc::Bytes(name.as_bytes())),
            (b"piece length", Enc::Int(piece_length as i64)),
            (b"pieces", Enc::Bytes(&pieces_blob)),
        ]),
    )])
}

/// Build a minimal v2 single-file torrent.
#[must_use]
pub fn synth_v2_single(name: &str, length: u64, piece_length: u64) -> Vec<u8> {
    // Dummy 32-byte root.
    let root = [0x11_u8; 32];
    let leaf = Enc::Dict(vec![(
        b"",
        Enc::Dict(vec![
            (b"length", Enc::Int(length as i64)),
            (b"pieces root", Enc::Bytes(&root[..])),
        ]),
    )]);
    // File tree has a single top-level entry named `name` containing the leaf.
    let tree = Enc::Dict(vec![(name.as_bytes(), leaf)]);
    encode_root(&[(
        b"info",
        Enc::Dict(vec![
            (b"file tree", tree),
            (b"meta version", Enc::Int(2)),
            (b"name", Enc::Bytes(name.as_bytes())),
            (b"piece length", Enc::Int(piece_length as i64)),
        ]),
    )])
}

/// Build a hybrid single-file torrent (v1 pieces + v2 file tree).
#[must_use]
pub fn synth_hybrid_single(name: &str, length: u64, piece_length: u64) -> Vec<u8> {
    let num_pieces = length.div_ceil(piece_length).max(1) as usize;
    let pieces_blob = vec![0_u8; num_pieces * 20];
    let root = [0x22_u8; 32];
    let leaf = Enc::Dict(vec![(
        b"",
        Enc::Dict(vec![
            (b"length", Enc::Int(length as i64)),
            (b"pieces root", Enc::Bytes(&root[..])),
        ]),
    )]);
    let tree = Enc::Dict(vec![(name.as_bytes(), leaf)]);
    encode_root(&[(
        b"info",
        Enc::Dict(vec![
            (b"file tree", tree),
            (b"length", Enc::Int(length as i64)),
            (b"meta version", Enc::Int(2)),
            (b"name", Enc::Bytes(name.as_bytes())),
            (b"piece length", Enc::Int(piece_length as i64)),
            (b"pieces", Enc::Bytes(&pieces_blob)),
        ]),
    )])
}

/// Compute the SHA-1 info-hash of a torrent's info dict given the full file bytes.
pub fn expected_v1_info_hash(torrent: &[u8]) -> [u8; 20] {
    let info = extract_info(torrent);
    Sha1::digest(info).into()
}

/// Compute the SHA-256 info-hash of a torrent's info dict.
pub fn expected_v2_info_hash(torrent: &[u8]) -> [u8; 32] {
    let info = extract_info(torrent);
    Sha256::digest(info).into()
}

fn extract_info(torrent: &[u8]) -> &[u8] {
    let span = magpie_bt_bencode::dict_value_span(torrent, b"info")
        .unwrap()
        .expect("synthetic torrents always carry an info dict");
    &torrent[span]
}

// --- Mini-encoder (sorts dict keys for canonical output) ---

/// Encoding helper mirroring the bencode AST with sorted dict keys.
pub enum Enc<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Enc<'a>>),
    Dict(Vec<(&'a [u8], Enc<'a>)>),
}

fn encode_root(pairs: &[(&[u8], Enc<'_>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let dict = Enc::Dict(pairs.iter().map(|(k, v)| (*k, clone_enc(v))).collect());
    encode(&dict, &mut out);
    out
}

fn encode(v: &Enc<'_>, out: &mut Vec<u8>) {
    match v {
        Enc::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Enc::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Enc::List(items) => {
            out.push(b'l');
            for item in items {
                encode(item, out);
            }
            out.push(b'e');
        }
        Enc::Dict(entries) => {
            let mut sorted: Vec<_> = entries.iter().map(|(k, v)| (*k, clone_enc(v))).collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            // Reject duplicate keys — synthetic builders must not produce them.
            for w in sorted.windows(2) {
                assert!(
                    w[0].0 != w[1].0,
                    "duplicate key `{:?}` in synth dict",
                    w[0].0
                );
            }
            out.push(b'd');
            for (k, val) in &sorted {
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode(val, out);
            }
            out.push(b'e');
        }
    }
}

fn clone_enc<'a>(v: &Enc<'a>) -> Enc<'a> {
    match v {
        Enc::Int(i) => Enc::Int(*i),
        Enc::Bytes(b) => Enc::Bytes(b),
        Enc::List(l) => Enc::List(l.iter().map(clone_enc).collect()),
        Enc::Dict(d) => Enc::Dict(d.iter().map(|(k, v)| (*k, clone_enc(v))).collect()),
    }
}
