#![allow(clippy::missing_panics_doc)]
//! BitTorrent peer-ID generation (Azureus style).
//!
//! A peer-ID is a 20-byte identifier a client announces to trackers and sends
//! in the handshake. Magpie follows the widely-used Azureus convention:
//!
//! ```text
//! -CCVVVV-XXXXXXXXXXXX
//!  \_/\__/\__________/
//!   |   |       |
//!   |   |       12-byte random suffix (printable ASCII)
//!   |   4-byte version tag
//!   2-byte client code
//! ```
//!
//! # Example
//! ```
//! use magpie_bt_core::peer_id::PeerIdBuilder;
//!
//! let builder = PeerIdBuilder::magpie(*b"0001");
//! let a = builder.build();
//! let b = builder.build();
//!
//! assert_eq!(&a[0..8], b"-Mg0001-");
//! assert_ne!(&a[8..], &b[8..], "suffixes should differ");
//! ```

/// Length of a BitTorrent peer-ID in bytes.
pub const PEER_ID_LEN: usize = 20;

/// Length of the random suffix (bytes after the leading `-CCVVVV-`).
pub const SUFFIX_LEN: usize = 12;

/// Client code magpie uses in its peer-ID prefix.
///
/// The two-byte Azureus "client code" isn't standardised; we pick `Mg` to
/// match the crate family (`magpie-bt-*`). Consumers that want to appear as a
/// different client can build with [`PeerIdBuilder::new`].
pub const MAGPIE_CLIENT_CODE: [u8; 2] = *b"Mg";

/// Produces Azureus-style peer-IDs (`-CCVVVV-<12-byte-suffix>`).
///
/// Construct once with the desired client code + version tag, then call
/// [`PeerIdBuilder::build`] on each fresh session / torrent to obtain a new
/// peer-ID with a freshly randomised suffix.
#[derive(Debug, Clone, Copy)]
pub struct PeerIdBuilder {
    client_code: [u8; 2],
    version: [u8; 4],
}

impl PeerIdBuilder {
    /// Creates a builder with the supplied client code and version tag.
    ///
    /// Both fields should be printable ASCII, but this is not enforced — some
    /// ecosystem conventions use non-ASCII bytes in the version field.
    #[must_use]
    pub const fn new(client_code: [u8; 2], version: [u8; 4]) -> Self {
        Self {
            client_code,
            version,
        }
    }

    /// Convenience constructor using [`MAGPIE_CLIENT_CODE`].
    #[must_use]
    pub const fn magpie(version: [u8; 4]) -> Self {
        Self::new(MAGPIE_CLIENT_CODE, version)
    }

    /// Returns the prefix (`-CCVVVV-`, 8 bytes) this builder emits.
    #[must_use]
    pub const fn prefix(&self) -> [u8; 8] {
        [
            b'-',
            self.client_code[0],
            self.client_code[1],
            self.version[0],
            self.version[1],
            self.version[2],
            self.version[3],
            b'-',
        ]
    }

    /// Generates a fresh peer-ID by drawing a 12-byte random suffix from the
    /// OS entropy source.
    ///
    /// # Panics
    /// Panics if the operating system fails to provide entropy — treated as
    /// catastrophic by every serious library (std, `rand`, `getrandom`).
    #[must_use]
    pub fn build(&self) -> [u8; PEER_ID_LEN] {
        let mut out = [0_u8; PEER_ID_LEN];
        out[..8].copy_from_slice(&self.prefix());
        getrandom::fill(&mut out[8..]).expect("OS entropy source failed");
        out
    }

    /// Generates a peer-ID using the supplied suffix instead of random bytes.
    ///
    /// Primarily for deterministic tests; real sessions should use
    /// [`PeerIdBuilder::build`].
    #[must_use]
    pub fn build_with_suffix(&self, suffix: [u8; SUFFIX_LEN]) -> [u8; PEER_ID_LEN] {
        let mut out = [0_u8; PEER_ID_LEN];
        out[..8].copy_from_slice(&self.prefix());
        out[8..].copy_from_slice(&suffix);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_azureus() {
        let b = PeerIdBuilder::magpie(*b"0001");
        let id = b.build_with_suffix([0; 12]);
        assert_eq!(&id[..8], b"-Mg0001-");
        assert_eq!(id.len(), 20);
    }

    #[test]
    fn consecutive_builds_differ_in_suffix() {
        let b = PeerIdBuilder::magpie(*b"0001");
        let a = b.build();
        let c = b.build();
        assert_eq!(&a[..8], &c[..8], "prefix must be stable");
        assert_ne!(a[8..], c[8..], "suffixes should differ across calls");
    }

    #[test]
    fn prefix_method_matches_built_id() {
        let b = PeerIdBuilder::new(*b"xy", *b"1234");
        let prefix = b.prefix();
        let id = b.build();
        assert_eq!(&id[..8], &prefix);
        assert_eq!(&prefix, b"-xy1234-");
    }

    #[test]
    fn suffix_has_reasonable_entropy() {
        // 100 samples; count distinct suffixes. Any OS RNG should give 100.
        let b = PeerIdBuilder::magpie(*b"0001");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let id = b.build();
            let mut s = [0_u8; 12];
            s.copy_from_slice(&id[8..]);
            seen.insert(s);
        }
        assert_eq!(seen.len(), 100, "each call must yield a unique suffix");
    }
}
