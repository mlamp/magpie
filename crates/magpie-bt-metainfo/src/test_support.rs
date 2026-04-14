//! Semver-exempt. Not covered by this crate's stability guarantees.
//!
//! Gated behind the `test-support` Cargo feature. Deterministic synthetic
//! torrent generator for integration tests, soak harnesses, and fuzz seed
//! corpora that need realistic metainfo + content without pulling artifacts
//! off the network.
//!
//! Use cases:
//! - ≥100k-piece torrents for ADR-0005 linear-picker exercise in the M2 soak
//!   gate.
//! - Small synthetic ~5 MiB fixtures for interop docker scenarios and the
//!   magpie-only controlled-swarm gate.
//! - Reproducible piece-hash coverage across CI runs (fixed seed → same
//!   content → same info-hash).
//!
//! # Stability
//!
//! This module is deliberately excluded from semver. The function surface and
//! output format may change in any release without a major version bump. Use
//! it only from tests and CI harnesses; never from production code.

use sha1::{Digest as _, Sha1};

/// Metadata + buffers returned by [`synthetic_torrent_v1`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SyntheticTorrent {
    /// Bencode-encoded `.torrent` bytes. Feed to
    /// [`crate::parse`] to obtain a [`MetaInfo`].
    pub torrent: Vec<u8>,
    /// Raw file content matching the torrent's piece hashes. `len()` equals
    /// `piece_length * piece_count`.
    pub content: Vec<u8>,
    /// SHA-1 digest of the info dict — the v1 info hash.
    pub info_hash: [u8; 20],
    /// Piece length in bytes (always a power of two, BEP-52-safe).
    pub piece_length: u32,
    /// Piece count.
    pub piece_count: u32,
}

/// Lower bound on piece length. BEP 52 mandates at least 16 KiB; enforce here
/// so v1 fixtures stay v2-upgrade-compatible (PROJECT.md invariant).
pub const MIN_PIECE_LENGTH: u32 = 16 * 1024;

/// Build a deterministic single-file v1 torrent backed by PRNG content.
///
/// `piece_length` must be a power of two ≥ [`MIN_PIECE_LENGTH`] (BEP 52
/// invariant, enforced in v1 per PROJECT.md). `piece_count` must be ≥ 1.
/// `total_length` = `piece_length * piece_count` (no short-last-piece
/// fixtures — those are trivial to derive by slicing `content`).
///
/// # Panics
///
/// Panics if `piece_length` is not a power of two ≥ [`MIN_PIECE_LENGTH`], or
/// if `piece_count == 0`. The generator is test-only; callers know these
/// constraints up front, so a panic is preferable to a cluttered error path.
#[must_use]
pub fn synthetic_torrent_v1(
    name: &str,
    piece_length: u32,
    piece_count: u32,
    seed: u64,
) -> SyntheticTorrent {
    assert!(piece_length >= MIN_PIECE_LENGTH, "piece_length < 16 KiB");
    assert!(
        piece_length.is_power_of_two(),
        "piece_length not power of two"
    );
    assert!(piece_count > 0, "piece_count must be > 0");

    let total_length = u64::from(piece_length) * u64::from(piece_count);
    let total_usize = usize::try_from(total_length)
        .expect("synthetic content length exceeds usize on this target");
    let mut content = Vec::with_capacity(total_usize);
    fill_splitmix64(&mut content, total_usize, seed);
    debug_assert_eq!(content.len(), total_usize);

    // Compute concatenated piece hashes.
    let mut pieces = Vec::with_capacity(piece_count as usize * 20);
    for chunk in content.chunks(piece_length as usize) {
        let mut hasher = Sha1::new();
        hasher.update(chunk);
        pieces.extend_from_slice(&hasher.finalize());
    }

    // Bencode the info dict manually. Keys in ascending order (BEP 3).
    // info = { length, name, piece length, pieces }
    let mut info = Vec::with_capacity(pieces.len() + name.len() + 64);
    info.push(b'd');
    encode_key(&mut info, b"length");
    encode_int(&mut info, total_length);
    encode_key(&mut info, b"name");
    encode_bytes(&mut info, name.as_bytes());
    encode_key(&mut info, b"piece length");
    encode_int(&mut info, u64::from(piece_length));
    encode_key(&mut info, b"pieces");
    encode_bytes(&mut info, &pieces);
    info.push(b'e');

    // info_hash = SHA-1 over the exact info-dict bytes.
    let mut hasher = Sha1::new();
    hasher.update(&info);
    let info_hash: [u8; 20] = hasher.finalize().into();

    // Outer dict: { info: <info> }. No announce; tests wire one explicitly.
    let mut torrent = Vec::with_capacity(info.len() + 16);
    torrent.push(b'd');
    encode_key(&mut torrent, b"info");
    torrent.extend_from_slice(&info);
    torrent.push(b'e');

    SyntheticTorrent {
        torrent,
        content,
        info_hash,
        piece_length,
        piece_count,
    }
}

fn encode_key(out: &mut Vec<u8>, key: &[u8]) {
    encode_bytes(out, key);
}

fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(bytes);
}

fn encode_int(out: &mut Vec<u8>, n: u64) {
    out.push(b'i');
    out.extend_from_slice(n.to_string().as_bytes());
    out.push(b'e');
}

/// Deterministic byte stream from a splitmix64 PRNG. Same algorithm magpie's
/// choker uses for its optimistic draw, so the project already audits it;
/// good enough for test content (not cryptographic).
fn fill_splitmix64(out: &mut Vec<u8>, target_len: usize, seed: u64) {
    let mut state = seed;
    out.resize(target_len, 0);
    let mut i = 0;
    while i + 8 <= target_len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out[i..i + 8].copy_from_slice(&z.to_le_bytes());
        i += 8;
    }
    // Tail < 8 bytes: one more draw, truncated.
    if i < target_len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out[i..target_len].copy_from_slice(&z.to_le_bytes()[..target_len - i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InfoHash, parse};

    #[test]
    fn parses_back_into_metainfo() {
        let t = synthetic_torrent_v1("small.bin", MIN_PIECE_LENGTH, 4, 42);
        let parsed = parse(&t.torrent).expect("synthetic torrent must parse");
        assert_eq!(parsed.info.name, b"small.bin");
        assert_eq!(parsed.info.piece_length, u64::from(t.piece_length));
        assert!(parsed.info.is_v1_only());
        assert!(matches!(parsed.info_hash, InfoHash::V1(h) if h == t.info_hash));
    }

    #[test]
    fn content_length_matches_piece_count_times_piece_length() {
        let t = synthetic_torrent_v1("x", MIN_PIECE_LENGTH, 7, 1);
        assert_eq!(
            t.content.len() as u64,
            u64::from(t.piece_length) * u64::from(t.piece_count)
        );
    }

    #[test]
    fn piece_hashes_match_content_chunks() {
        let t = synthetic_torrent_v1("x", MIN_PIECE_LENGTH, 3, 7);
        let parsed = parse(&t.torrent).unwrap();
        let pieces_field = parsed.info.v1.as_ref().unwrap().pieces;
        assert_eq!(pieces_field.len(), 3 * 20);
        for (i, chunk) in t.content.chunks(MIN_PIECE_LENGTH as usize).enumerate() {
            let mut h = Sha1::new();
            h.update(chunk);
            let expected: [u8; 20] = h.finalize().into();
            assert_eq!(&pieces_field[i * 20..(i + 1) * 20], &expected);
        }
    }

    #[test]
    fn same_seed_same_content() {
        let a = synthetic_torrent_v1("a", MIN_PIECE_LENGTH, 2, 0xDEAD_BEEF);
        let b = synthetic_torrent_v1("a", MIN_PIECE_LENGTH, 2, 0xDEAD_BEEF);
        assert_eq!(a.content, b.content);
        assert_eq!(a.info_hash, b.info_hash);
    }

    #[test]
    fn different_seeds_different_content() {
        let a = synthetic_torrent_v1("a", MIN_PIECE_LENGTH, 2, 1);
        let b = synthetic_torrent_v1("a", MIN_PIECE_LENGTH, 2, 2);
        assert_ne!(a.content, b.content);
        assert_ne!(a.info_hash, b.info_hash);
    }

    #[test]
    #[should_panic(expected = "piece_length < 16 KiB")]
    fn rejects_too_small_piece() {
        let _ = synthetic_torrent_v1("x", 8 * 1024, 1, 0);
    }

    #[test]
    #[should_panic(expected = "piece_length not power of two")]
    fn rejects_non_power_of_two_piece() {
        let _ = synthetic_torrent_v1("x", MIN_PIECE_LENGTH + 1, 1, 0);
    }

    #[test]
    #[should_panic(expected = "piece_count must be > 0")]
    fn rejects_zero_pieces() {
        let _ = synthetic_torrent_v1("x", MIN_PIECE_LENGTH, 0, 0);
    }
}
