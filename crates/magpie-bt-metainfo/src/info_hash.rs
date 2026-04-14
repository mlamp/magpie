//! `InfoHash` — the 20- or 32-byte digest identifying a torrent.

use sha1::{Digest as _, Sha1};
use sha2::Sha256;

/// Info-hash of a torrent.
///
/// - [`InfoHash::V1`] — SHA-1(bencode(info)) for BEP 3 torrents.
/// - [`InfoHash::V2`] — SHA-256(bencode(info)) for BEP 52 v2-only torrents.
/// - [`InfoHash::Hybrid`] — both digests computed over the same info dict,
///   per the BEP 52 hybrid format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InfoHash {
    /// v1 SHA-1 digest (20 bytes).
    V1([u8; 20]),
    /// v2 SHA-256 digest (32 bytes).
    V2([u8; 32]),
    /// Both digests are valid for the same info dict.
    Hybrid {
        /// v1 SHA-1 digest.
        v1: [u8; 20],
        /// v2 SHA-256 digest.
        v2: [u8; 32],
    },
}

impl InfoHash {
    /// Returns the v1 digest if this hash carries one.
    #[must_use]
    pub const fn v1(&self) -> Option<&[u8; 20]> {
        match self {
            Self::V1(h) | Self::Hybrid { v1: h, .. } => Some(h),
            Self::V2(_) => None,
        }
    }

    /// Returns the v2 digest if this hash carries one.
    #[must_use]
    pub const fn v2(&self) -> Option<&[u8; 32]> {
        match self {
            Self::V2(h) | Self::Hybrid { v2: h, .. } => Some(h),
            Self::V1(_) => None,
        }
    }

    /// Returns `true` when both v1 and v2 digests are present.
    #[must_use]
    pub const fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid { .. })
    }
}

/// Computes SHA-1 over the given bytes.
#[must_use]
pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Computes SHA-256 over the given bytes.
#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}
