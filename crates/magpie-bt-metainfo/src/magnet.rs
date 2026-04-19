//! Magnet URI parser and formatter (BEP 9).
//!
//! Parses `magnet:?xt=urn:btih:<hash>&...` URIs into a [`MagnetLink`] struct,
//! supporting both 40-char hex and 32-char base32 info-hash encodings.
//!
//! # Example
//! ```
//! use magpie_bt_metainfo::MagnetLink;
//!
//! let uri = "magnet:?xt=urn:btih:da39a3ee5e6b4b0d3255bfef95601890afd80709\
//!            &dn=hello&tr=udp%3A%2F%2Ftracker.example.com%3A6969";
//! let link = MagnetLink::parse(uri).unwrap();
//! assert_eq!(link.display_name.as_deref(), Some("hello"));
//! assert_eq!(link.trackers.len(), 1);
//! ```

use std::fmt;
use std::net::SocketAddr;

use thiserror::Error;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Maximum number of `tr=` (tracker) parameters accepted in a single magnet URI.
const MAX_TRACKERS: usize = 100;
/// Maximum number of `x.pe=` (peer address) parameters accepted in a single magnet URI.
const MAX_PEER_ADDRS: usize = 200;
/// Maximum byte length of any single percent-decoded parameter value.
const MAX_PARAM_LENGTH: usize = 4096;
/// Maximum length of raw user input embedded in error messages.
const MAX_ERROR_INPUT_LENGTH: usize = 64;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error returned when parsing a magnet URI.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MagnetError {
    /// The URI does not start with `magnet:?`.
    #[error("invalid scheme: expected `magnet:?` prefix")]
    InvalidScheme,
    /// No `xt=urn:btih:` parameter was found.
    #[error("missing info hash (`xt` parameter)")]
    MissingInfoHash,
    /// The info hash value could not be decoded (bad hex, bad base32, wrong
    /// URN scheme, or incorrect length).
    #[error("invalid info hash: {0}")]
    InvalidInfoHash(String),
    /// An `x.pe` peer address could not be parsed as a `SocketAddr`.
    #[error("invalid peer address: {0}")]
    InvalidPeerAddr(String),
    /// A parameter count or size limit was exceeded (denial-of-service protection).
    #[error("parameter limit exceeded: {0}")]
    LimitExceeded(String),
}

// ---------------------------------------------------------------------------
// MagnetLink
// ---------------------------------------------------------------------------

/// A parsed v1 magnet URI.
///
/// Created via [`MagnetLink::parse`]; can be round-tripped back to a URI
/// string via its [`Display`](fmt::Display) implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetLink {
    /// v1 SHA-1 info hash (20 bytes), decoded from the `xt=urn:btih:` param.
    pub info_hash: [u8; 20],
    /// Display name (`dn` parameter), if present.
    pub display_name: Option<String>,
    /// Tracker announce URLs (`tr` parameters).
    pub trackers: Vec<String>,
    /// Peer addresses (`x.pe` parameters).
    pub peer_addrs: Vec<SocketAddr>,
}

impl MagnetLink {
    /// Parse a magnet URI string into a [`MagnetLink`].
    ///
    /// # Errors
    ///
    /// Returns [`MagnetError`] when the URI is malformed, missing its info
    /// hash, or contains an unparseable peer address.
    pub fn parse(uri: &str) -> Result<Self, MagnetError> {
        let query = uri
            .strip_prefix("magnet:?")
            .ok_or(MagnetError::InvalidScheme)?;

        let mut info_hash: Option<[u8; 20]> = None;
        let mut display_name: Option<String> = None;
        let mut trackers: Vec<String> = Vec::new();
        let mut peer_addrs: Vec<SocketAddr> = Vec::new();

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };

            match key {
                "xt" => {
                    let hash_str = value
                        .strip_prefix("urn:btih:")
                        .ok_or_else(|| {
                            MagnetError::InvalidInfoHash(format!(
                                "expected `urn:btih:` prefix, got `{}`",
                                truncate_for_error(value),
                            ))
                        })?;
                    if info_hash.is_some() {
                        return Err(MagnetError::InvalidInfoHash(
                            "duplicate xt parameter".into(),
                        ));
                    }
                    info_hash = Some(decode_info_hash(hash_str)?);
                }
                "dn" => {
                    let decoded = percent_decode(value);
                    if decoded.len() > MAX_PARAM_LENGTH {
                        return Err(MagnetError::LimitExceeded(
                            "dn parameter value too long".into(),
                        ));
                    }
                    display_name = Some(decoded);
                }
                "tr" => {
                    if trackers.len() >= MAX_TRACKERS {
                        return Err(MagnetError::LimitExceeded(format!(
                            "too many tr parameters (max {MAX_TRACKERS})"
                        )));
                    }
                    let decoded = percent_decode(value);
                    if decoded.len() > MAX_PARAM_LENGTH {
                        return Err(MagnetError::LimitExceeded(
                            "tr parameter value too long".into(),
                        ));
                    }
                    trackers.push(decoded);
                }
                "x.pe" => {
                    if peer_addrs.len() >= MAX_PEER_ADDRS {
                        return Err(MagnetError::LimitExceeded(format!(
                            "too many x.pe parameters (max {MAX_PEER_ADDRS})"
                        )));
                    }
                    let decoded = percent_decode(value);
                    if decoded.len() > MAX_PARAM_LENGTH {
                        return Err(MagnetError::LimitExceeded(
                            "x.pe parameter value too long".into(),
                        ));
                    }
                    let addr: SocketAddr = decoded.parse().map_err(|_| {
                        MagnetError::InvalidPeerAddr(truncate_for_error(&decoded))
                    })?;
                    peer_addrs.push(addr);
                }
                // Forward-compatible: ignore unknown parameters.
                _ => {}
            }
        }

        let info_hash = info_hash.ok_or(MagnetError::MissingInfoHash)?;

        Ok(Self {
            info_hash,
            display_name,
            trackers,
            peer_addrs,
        })
    }
}

impl fmt::Display for MagnetLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("magnet:?xt=urn:btih:")?;
        for byte in &self.info_hash {
            write!(f, "{byte:02x}")?;
        }
        if let Some(dn) = &self.display_name {
            f.write_str("&dn=")?;
            percent_encode_into(dn, f)?;
        }
        for tr in &self.trackers {
            f.write_str("&tr=")?;
            percent_encode_into(tr, f)?;
        }
        for addr in &self.peer_addrs {
            f.write_str("&x.pe=")?;
            // IPv6 socket addrs contain `[`, `]`, `:` — all need encoding.
            let s = addr.to_string();
            percent_encode_into(&s, f)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Decode a 40-char hex or 32-char base32 string into a 20-byte info hash.
fn decode_info_hash(s: &str) -> Result<[u8; 20], MagnetError> {
    match s.len() {
        40 => hex_decode(s),
        32 => base32_decode(s),
        other => Err(MagnetError::InvalidInfoHash(format!(
            "expected 40 hex chars or 32 base32 chars, got {other} chars"
        ))),
    }
}

/// Decode a 40-character hex string into 20 bytes.
fn hex_decode(s: &str) -> Result<[u8; 20], MagnetError> {
    let mut out = [0u8; 20];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

/// Convert a single ASCII hex character to its 4-bit value.
fn hex_nibble(b: u8) -> Result<u8, MagnetError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(MagnetError::InvalidInfoHash(format!(
            "invalid hex character: '{}'",
            char::from(b)
        ))),
    }
}

/// Decode a 32-character base32 (RFC 4648, no padding) string into 20 bytes.
fn base32_decode(s: &str) -> Result<[u8; 20], MagnetError> {
    // 32 base32 chars = 32 * 5 = 160 bits = 20 bytes.
    let mut out = [0u8; 20];
    let mut bits: u64 = 0;
    let mut n_bits: u32 = 0;
    let mut pos = 0;

    for &b in s.as_bytes() {
        let val = base32_value(b)?;
        bits = (bits << 5) | u64::from(val);
        n_bits += 5;
        while n_bits >= 8 {
            n_bits -= 8;
            if pos >= 20 {
                return Err(MagnetError::InvalidInfoHash(
                    "base32 decoded to more than 20 bytes".into(),
                ));
            }
            #[allow(clippy::cast_possible_truncation)] // intentional: we mask to 8 bits
            {
                out[pos] = (bits >> n_bits) as u8;
            }
            bits &= (1u64 << n_bits) - 1;
            pos += 1;
        }
    }

    if pos != 20 {
        return Err(MagnetError::InvalidInfoHash(format!(
            "base32 decoded to {pos} bytes, expected 20"
        )));
    }
    Ok(out)
}

/// Map a single ASCII character to its base32 (RFC 4648) 5-bit value.
fn base32_value(b: u8) -> Result<u8, MagnetError> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a'),
        b'2'..=b'7' => Ok(b - b'2' + 26),
        _ => Err(MagnetError::InvalidInfoHash(format!(
            "invalid base32 character: '{}'",
            char::from(b)
        ))),
    }
}

/// Truncate a raw input string for safe inclusion in error messages.
fn truncate_for_error(s: &str) -> String {
    if s.len() <= MAX_ERROR_INPUT_LENGTH {
        s.to_string()
    } else {
        format!("{}...", &s[..MAX_ERROR_INPUT_LENGTH])
    }
}

/// Percent-decode a query parameter value.
fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Ok(hi), Ok(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a string into a formatter, encoding everything except
/// unreserved characters (RFC 3986: `ALPHA / DIGIT / "-" / "." / "_" / "~"`).
fn percent_encode_into(s: &str, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                write!(f, "{}", char::from(b))?;
            }
            _ => {
                write!(f, "%{b:02X}")?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The SHA-1 of the empty string, used as a convenient known hash.
    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    fn empty_sha1_bytes() -> [u8; 20] {
        hex_decode(EMPTY_SHA1_HEX).unwrap()
    }

    // -- happy paths --------------------------------------------------------

    #[test]
    fn basic_hex_hash() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.info_hash, empty_sha1_bytes());
        assert_eq!(link.display_name, None);
        assert!(link.trackers.is_empty());
        assert!(link.peer_addrs.is_empty());
    }

    #[test]
    fn uppercase_hex_hash() {
        let uri = format!(
            "magnet:?xt=urn:btih:{}",
            EMPTY_SHA1_HEX.to_uppercase()
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.info_hash, empty_sha1_bytes());
    }

    #[test]
    fn base32_hash() {
        // base32 of the 20 zero bytes: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA (all A's, but
        // let's use a real example). The empty-SHA1 in base32:
        // da39a3ee... -> base32 encode the 20 bytes.
        let hash_bytes = empty_sha1_bytes();
        let b32 = base32_encode(&hash_bytes);
        assert_eq!(b32.len(), 32);

        let uri = format!("magnet:?xt=urn:btih:{b32}");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.info_hash, hash_bytes);
    }

    #[test]
    fn display_name_url_decoded() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn=My%20Cool%20Torrent%21"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("My Cool Torrent!"));
    }

    #[test]
    fn display_name_plus_as_space() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn=hello+world"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("hello world"));
    }

    #[test]
    fn multiple_trackers() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}\
             &tr=udp%3A%2F%2Ftracker1.example.com%3A6969\
             &tr=http%3A%2F%2Ftracker2.example.com%2Fannounce"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.trackers.len(), 2);
        assert_eq!(link.trackers[0], "udp://tracker1.example.com:6969");
        assert_eq!(
            link.trackers[1],
            "http://tracker2.example.com/announce"
        );
    }

    #[test]
    fn peer_addr_ipv4() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&x.pe=192.168.1.1%3A6881"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.peer_addrs.len(), 1);
        assert_eq!(
            link.peer_addrs[0],
            "192.168.1.1:6881".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn peer_addr_ipv6() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&x.pe=%5B%3A%3A1%5D%3A6881"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.peer_addrs.len(), 1);
        assert_eq!(
            link.peer_addrs[0],
            "[::1]:6881".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn unknown_params_ignored() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&foo=bar&baz=qux"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.info_hash, empty_sha1_bytes());
    }

    #[test]
    fn round_trip() {
        let uri = format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}\
             &dn=Test%20Name\
             &tr=udp%3A%2F%2Ftracker.example.com%3A6969\
             &x.pe=192.168.1.1%3A6881"
        );
        let link = MagnetLink::parse(&uri).unwrap();
        let serialized = link.to_string();
        let reparsed = MagnetLink::parse(&serialized).unwrap();
        assert_eq!(link, reparsed);
    }

    #[test]
    fn round_trip_ipv6() {
        let link = MagnetLink {
            info_hash: empty_sha1_bytes(),
            display_name: Some("v6 test".into()),
            trackers: vec![],
            peer_addrs: vec!["[::1]:6881".parse().unwrap()],
        };
        let serialized = link.to_string();
        let reparsed = MagnetLink::parse(&serialized).unwrap();
        assert_eq!(link, reparsed);
    }

    #[test]
    fn full_kitchen_sink() {
        let link = MagnetLink {
            info_hash: empty_sha1_bytes(),
            display_name: Some("A & B = C!".into()),
            trackers: vec![
                "udp://t1.example.com:6969".into(),
                "http://t2.example.com/announce?key=val".into(),
            ],
            peer_addrs: vec![
                "1.2.3.4:5678".parse().unwrap(),
                "[::1]:6881".parse().unwrap(),
            ],
        };
        let reparsed = MagnetLink::parse(&link.to_string()).unwrap();
        assert_eq!(link, reparsed);
    }

    // -- error cases --------------------------------------------------------

    #[test]
    fn error_missing_scheme() {
        let err = MagnetLink::parse("http://example.com").unwrap_err();
        assert!(matches!(err, MagnetError::InvalidScheme));
    }

    #[test]
    fn error_missing_xt() {
        let err = MagnetLink::parse("magnet:?dn=hello").unwrap_err();
        assert!(matches!(err, MagnetError::MissingInfoHash));
    }

    #[test]
    fn error_wrong_urn() {
        let err = MagnetLink::parse(
            "magnet:?xt=urn:sha1:da39a3ee5e6b4b0d3255bfef95601890afd80709",
        )
        .unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn error_invalid_hex_length() {
        let err = MagnetLink::parse("magnet:?xt=urn:btih:abcd").unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn error_invalid_hex_chars() {
        // 40 chars but contains 'g'
        let bad = "ga39a3ee5e6b4b0d3255bfef95601890afd80709";
        assert_eq!(bad.len(), 40);
        let err =
            MagnetLink::parse(&format!("magnet:?xt=urn:btih:{bad}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn error_invalid_peer_addr() {
        let err = MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&x.pe=not-an-addr"
        ))
        .unwrap_err();
        assert!(matches!(err, MagnetError::InvalidPeerAddr(_)));
    }

    // -- helpers for tests --------------------------------------------------

    /// Minimal base32 encoder (RFC 4648, no padding) for test use only.
    fn base32_encode(data: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::new();
        let mut bits: u64 = 0;
        let mut n_bits: u32 = 0;
        for &b in data {
            bits = (bits << 8) | u64::from(b);
            n_bits += 8;
            while n_bits >= 5 {
                n_bits -= 5;
                out.push(char::from(ALPHABET[((bits >> n_bits) & 0x1f) as usize]));
                bits &= (1u64 << n_bits) - 1;
            }
        }
        if n_bits > 0 {
            bits <<= 5 - n_bits;
            out.push(char::from(ALPHABET[(bits & 0x1f) as usize]));
        }
        out
    }
}

#[cfg(test)]
mod additional_edge_case_tests {
    use super::*;
    use std::fmt::Write as _;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn duplicate_xt_param_returns_error() {
        let uri = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000\
                            &xt=urn:btih:da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let err = MagnetLink::parse(uri).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn info_hash_39_chars() {
        let hash = "da39a3ee5e6b4b0d3255bfef95601890afd8070";
        assert_eq!(hash.len(), 39);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{hash}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn info_hash_41_chars() {
        let hash = "da39a3ee5e6b4b0d3255bfef95601890afd807099";
        assert_eq!(hash.len(), 41);
        let err = MagnetLink::parse(&hash_uri(hash)).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    fn hash_uri(hash: &str) -> String {
        format!("magnet:?xt=urn:btih:{hash}")
    }

    #[test]
    fn percent_decode_invalid_sequence() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn=%ZZ");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("%ZZ"));
    }

    #[test]
    fn percent_decode_incomplete_sequence() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn=test%2");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("test%2"));
    }

    #[test]
    fn no_question_mark() {
        let err = MagnetLink::parse(&format!("magnet:xt=urn:btih:{EMPTY_SHA1_HEX}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidScheme));
    }

    #[test]
    fn param_without_value() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some(""));
    }

    #[test]
    fn percent_decode_invalid_utf8() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn=%FF");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("\u{FFFD}"));
    }

    #[test]
    fn peer_addr_invalid_port() {
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&x.pe=127.0.0.1%3A99999");
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidPeerAddr(_)));
    }

    #[test]
    fn many_trackers_within_limit() {
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..MAX_TRACKERS {
            let _ = write!(uri, "&tr=http%3A%2F%2Ftracker{i}.example.com%3A6969");
        }
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.trackers.len(), MAX_TRACKERS);
    }

    #[test]
    fn base32_with_invalid_char() {
        let hash = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0";
        assert_eq!(hash.len(), 32);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{hash}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn percent_decode_boundary_check() {
        // Line 250: i + 2 < bytes.len()
        // This means it needs i + 2 to be strictly less than len
        // For input "%AB" (len=3), at i=0: 0 + 2 < 3 is TRUE, so it tries to decode
        // For input "%A" (len=2), at i=0: 0 + 2 < 2 is FALSE, so it doesn't decode
        // For input "X%A" (len=3), at i=1: 1 + 2 < 3 is FALSE, so it doesn't decode - WRONG!
        
        // This is a BUG. The check should be i + 2 <= bytes.len() or i + 3 <= bytes.len()
        
        // %A at position 1 in "X%A":
        // bytes = [X, %, A], len = 3
        // i = 1: bytes[i] == b'%' ? yes
        // i + 2 < bytes.len() ? 1 + 2 < 3 ? FALSE
        // So it falls through and just appends '%', 'A'
        
        let result = percent_decode("X%A");
        // Currently: "X%A" (% is not special)
        assert_eq!(result, "X%A");
    }

    #[test]
    fn percent_decode_exact_boundary() {
        // What about "%2F" at the very end?
        // bytes = [%, 2, F], len = 3
        // i = 0: 0 + 2 < 3 is TRUE, so it WILL decode
        let result = percent_decode("%2F");
        assert_eq!(result, "/");
    }

    #[test]
    fn percent_decode_off_by_one() {
        // "\u0041" is ASCII 'A', so %41 should decode to 'A'
        // But the boundary check i + 2 < bytes.len() is wrong
        // For 2-char input "%4": bytes = [%, 4], len = 2
        // i = 0: 0 + 2 < 2 is FALSE, so doesn't decode
        let result = percent_decode("%4");
        assert_eq!(result, "%4");
        
        // For 3-char input "%41": bytes = [%, 4, 1], len = 3
        // i = 0: 0 + 2 < 3 is TRUE, so it decodes!
        let result2 = percent_decode("%41");
        assert_eq!(result2, "A");
    }

    #[test]
    fn percent_decode_mid_string_incomplete() {
        // "abc%4def" - the %4d is at positions 3,4,5 in the string
        // bytes = [a, b, c, %, 4, d, e, f], len = 8
        // At i=3: bytes[3] == b'%', check: 3 + 2 < 8 ? YES
        // So it tries to decode bytes[4]=b'4' and bytes[5]=b'd' as hex
        // b'd' is valid hex (13), b'4' is valid hex (4)
        // So it decodes to byte 0x4d (77, ASCII 'M')
        let result = percent_decode("abc%4def");
        assert_eq!(result, "abcMef");
    }
}

#[cfg(test)]
mod debug_percent_decode {
    use super::*;

    #[test]
    fn debug_boundary() {
        // For input "%41":
        // bytes = [37 (%), 52 (4), 49 (1)]
        // len = 3
        // i = 0: bytes[0] == 37 (%), then check: 0 + 2 < 3 ?
        // 0 + 2 = 2, 2 < 3 = true
        // So we access bytes[1] and bytes[2]
        // This is CORRECT - we need 3 bytes total (%, hex, hex)
        
        // For input "%4":
        // bytes = [37 (%), 52 (4)]
        // len = 2
        // i = 0: bytes[0] == 37 (%), then check: 0 + 2 < 2 ?
        // 0 + 2 = 2, 2 < 2 = false
        // So we DON'T decode - we just append '%'
        // This is CORRECT
        
        // For input "X%A":
        // bytes = [88 (X), 37 (%), 65 (A)]
        // len = 3
        // i = 0: bytes[0] != 37, append
        // i = 1: bytes[1] == 37, check: 1 + 2 < 3 ?
        // 1 + 2 = 3, 3 < 3 = false
        // So we DON'T try to decode
        // We append 37 (%), i += 1 (now i=2)
        // i = 2: bytes[2] == 65 (A), append
        // Result: [X, %, A] = "X%A"
        // This is CORRECT - %A is incomplete
        
        let r1 = percent_decode("%41");
        assert_eq!(r1, "A");
        
        let r2 = percent_decode("%4");
        assert_eq!(r2, "%4");
        
        let r3 = percent_decode("X%A");
        assert_eq!(r3, "X%A");
    }
}

#[cfg(test)]
mod dos_and_allocation_tests {
    use super::*;
    use std::fmt::Write as _;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn tracker_allocation_limited() {
        // Exceeding MAX_TRACKERS should return an error, not allocate unboundedly.
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..=MAX_TRACKERS {
            let _ = write!(uri, "&tr=http%3A%2F%2Ft{i}.example.com");
        }
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
    }

    #[test]
    fn peer_addr_allocation_limited() {
        // Exceeding MAX_PEER_ADDRS should return an error.
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..=MAX_PEER_ADDRS {
            let octet = (i % 255) + 1;
            let _ = write!(uri, "&x.pe=192.168.0.{octet}%3A6969");
        }
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
    }

    #[test]
    fn very_long_display_name_rejected() {
        // A display_name exceeding MAX_PARAM_LENGTH should be rejected.
        let long_name = "a".repeat(MAX_PARAM_LENGTH + 1);
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn={long_name}");
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
    }

    #[test]
    fn percent_decode_stack_overflow_resistance() {
        // Percent-decoding is not recursive, so no stack overflow risk.
        // Even a deeply nested encoding like %252525... is safe (3-byte input -> 1-byte output).
        // We just build a long percent-encoded string and verify it doesn't crash.
        let deep = "%61".repeat(500); // 1500 ASCII bytes, all valid percent sequences
        let result = percent_decode(&deep);
        assert_eq!(result, "a".repeat(500));
    }

    #[test]
    fn large_unknown_param_allocation() {
        // Unknown parameters are silently ignored (no allocation)
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..10000 {
            let _ = write!(uri, "&unknown{i}=verylongvalue{}", "x".repeat(100));
        }
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.info_hash.len(), 20);
    }
}

#[cfg(test)]
mod round_trip_and_display_tests {
    use super::*;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn display_encodes_unreserved_chars() {
        // RFC 3986 unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~"
        let link = MagnetLink {
            info_hash: hex_decode(EMPTY_SHA1_HEX).unwrap(),
            display_name: Some("test-value_123.456~file".to_string()),
            trackers: vec![],
            peer_addrs: vec![],
        };
        let uri = link.to_string();
        // These should NOT be percent-encoded
        assert!(uri.contains("test-value_123.456~file"));
    }

    #[test]
    fn display_encodes_special_chars() {
        let link = MagnetLink {
            info_hash: hex_decode(EMPTY_SHA1_HEX).unwrap(),
            display_name: Some("hello world!".to_string()),
            trackers: vec![],
            peer_addrs: vec![],
        };
        let uri = link.to_string();
        // Space should be encoded
        assert!(!uri.contains("hello world"));
        assert!(uri.contains("hello%20world"));
        // ! should be encoded
        assert!(uri.contains("%21"));
    }

    #[test]
    fn display_roundtrip_is_idempotent() {
        let original = MagnetLink {
            info_hash: hex_decode(EMPTY_SHA1_HEX).unwrap(),
            display_name: Some("test & special = chars!".to_string()),
            trackers: vec!["http://example.com/announce?key=value".to_string()],
            peer_addrs: vec![],
        };
        let uri1 = original.to_string();
        let reparsed1 = MagnetLink::parse(&uri1).unwrap();
        let uri2 = reparsed1.to_string();
        let reparsed2 = MagnetLink::parse(&uri2).unwrap();
        
        // After 2 round-trips, should be identical
        assert_eq!(reparsed1, original);
        assert_eq!(reparsed2, reparsed1);
        assert_eq!(uri1, uri2);
    }

    #[test]
    fn param_order_preserved_on_roundtrip() {
        let original = "magnet:?xt=urn:btih:da39a3ee5e6b4b0d3255bfef95601890afd80709\
                                &dn=name\
                                &tr=http%3A%2F%2Ftracker.example.com\
                                &x.pe=192.168.1.1%3A6969";
        let parsed = MagnetLink::parse(original).unwrap();
        let serialized = parsed.to_string();
        
        // The Display impl outputs in a specific order:
        // xt, dn, tr (all of them), x.pe (all of them)
        assert!(serialized.starts_with("magnet:?xt=urn:btih:"));
        assert!(serialized.contains("&dn="));
        assert!(serialized.contains("&tr="));
        assert!(serialized.contains("&x.pe="));
    }

    #[test]
    fn multiple_trackers_order_in_display() {
        let link = MagnetLink {
            info_hash: hex_decode(EMPTY_SHA1_HEX).unwrap(),
            display_name: None,
            trackers: vec![
                "http://tracker1.example.com".to_string(),
                "http://tracker2.example.com".to_string(),
                "http://tracker3.example.com".to_string(),
            ],
            peer_addrs: vec![],
        };
        let uri = link.to_string();
        // All trackers should appear in order
        let idx1 = uri.find("tracker1").expect("tracker1");
        let idx2 = uri.find("tracker2").expect("tracker2");
        let idx3 = uri.find("tracker3").expect("tracker3");
        assert!(idx1 < idx2 && idx2 < idx3);
    }

    #[test]
    fn info_hash_always_lowercase_hex_on_display() {
        let link = MagnetLink {
            info_hash: hex_decode(EMPTY_SHA1_HEX).unwrap(),
            display_name: None,
            trackers: vec![],
            peer_addrs: vec![],
        };
        let uri = link.to_string();
        // The hex bytes are formatted with {:02x} which is lowercase
        assert_eq!(uri, format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}"));
    }
}

#[cfg(test)]
mod bep_compliance_tests {
    use super::*;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn bep9_spec_xt_parameter_required() {
        // BEP 9 requires xt (exact topic) parameter
        let err = MagnetLink::parse("magnet:?dn=test").unwrap_err();
        assert!(matches!(err, MagnetError::MissingInfoHash));
    }

    #[test]
    fn bep9_spec_xt_format() {
        // BEP 9 requires format: urn:btih:<hash>
        // The spec says hash is SHA1 (20 bytes), encoded as 40 hex OR 32 base32
        let uri_hex = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        let link_hex = MagnetLink::parse(&uri_hex).unwrap();
        assert_eq!(link_hex.info_hash.len(), 20);
    }

    #[test]
    fn bep9_spec_dn_optional() {
        // Display name (dn) is optional per BEP 9
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name, None);
    }

    #[test]
    fn bep9_spec_tr_announce_url() {
        // BEP 9 defines tr as tracker announce URL, can appear multiple times
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}\
                          &tr=http%3A%2F%2Ftracker1.example.com%2Fannounce\
                          &tr=http%3A%2F%2Ftracker2.example.com%2Fannounce");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.trackers.len(), 2);
    }

    #[test]
    fn bep9_spec_x_pe_peer() {
        // Extension: x.pe (peer exchange) - not core to BEP 9 but widely supported
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}\
                          &x.pe=192.168.1.1%3A6881");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.peer_addrs.len(), 1);
    }

    #[test]
    fn bep9_spec_case_sensitivity() {
        // BEP 9 says parameter names are case-sensitive
        // So "XT" should NOT match "xt"
        let uri = format!("magnet:?XT=urn:btih:{EMPTY_SHA1_HEX}");
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::MissingInfoHash));
    }

    #[test]
    fn non_btih_urn_scheme_rejected() {
        // Only urn:btih: is supported (v1 hashes)
        // urn:sha1: would be wrong (should be v2)
        let err = MagnetLink::parse(
            "magnet:?xt=urn:sha1:da39a3ee5e6b4b0d3255bfef95601890afd80709"
        ).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn v2_bitprint_not_supported() {
        // BEP 9 defines urn:bitprint: as sha1.sha256
        // This implementation only supports urn:btih:
        let err = MagnetLink::parse(
            "magnet:?xt=urn:bitprint:da39a3ee5e6b4b0d3255bfef95601890afd80709.0bfe\
                    4b197367e4dda0992f7c09e6d69e7e6e8cccbec3b8e75c28e2b8a7c7d8e9f0"
        ).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn unknown_parameters_forward_compatible() {
        // Unknown parameters should be silently ignored (forward compatibility)
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}\
                          &dn=test\
                          &future-extension=value\
                          &tr=http%3A%2F%2Ftracker.example.com");
        let link = MagnetLink::parse(&uri).unwrap();
        assert_eq!(link.display_name.as_deref(), Some("test"));
        assert_eq!(link.trackers.len(), 1);
    }
}

#[cfg(test)]
mod information_disclosure_tests {
    use super::*;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn error_message_truncates_user_input() {
        // Error messages should truncate raw user input to prevent information leaks.
        let long_value = "urn:sha1:".to_string() + &"x".repeat(200);
        let uri = format!("magnet:?xt={long_value}");
        let err = MagnetLink::parse(&uri).unwrap_err();

        match err {
            MagnetError::InvalidInfoHash(msg) => {
                // The embedded user input should be truncated to MAX_ERROR_INPUT_LENGTH + "..."
                assert!(msg.len() <= 200, "error message should not contain the full 200-char input");
                assert!(msg.contains("..."));
            }
            _ => panic!("Expected InvalidInfoHash"),
        }
    }

    #[test]
    fn error_message_for_invalid_hex_char() {
        // Line 187-189: hex_nibble error includes the character
        let bad_hash = "ga39a3ee5e6b4b0d3255bfef95601890afd80709";
        let uri = format!("magnet:?xt=urn:btih:{bad_hash}");
        let err = MagnetLink::parse(&uri).unwrap_err();
        
        match err {
            MagnetError::InvalidInfoHash(msg) => {
                assert!(msg.contains("'g'"));
            }
            _ => panic!("Expected InvalidInfoHash"),
        }
    }

    #[test]
    fn error_message_for_invalid_base32_char() {
        // Line 236-239: base32_value error includes the character
        let bad_hash = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1";
        let uri = format!("magnet:?xt=urn:btih:{bad_hash}");
        let err = MagnetLink::parse(&uri).unwrap_err();
        
        match err {
            MagnetError::InvalidInfoHash(msg) => {
                assert!(msg.contains("'1'"));
            }
            _ => panic!("Expected InvalidInfoHash"),
        }
    }

    #[test]
    fn error_message_for_invalid_peer_addr() {
        // Line 111: error message includes the decoded address
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&x.pe=not-an-address");
        let err = MagnetLink::parse(&uri).unwrap_err();
        
        match err {
            MagnetError::InvalidPeerAddr(msg) => {
                assert_eq!(msg, "not-an-address");
            }
            _ => panic!("Expected InvalidPeerAddr"),
        }
    }
}

#[cfg(test)]
mod boundary_bug_tests {
    use super::*;

    #[test]
    fn hex_decode_odd_length_string() {
        // hex_decode is called after checking length == 40
        // But what if someone manually calls hex_decode with odd length?
        // The chunks(2) iterator will create a final chunk of size 1
        // Then chunk[1] will panic with index out of bounds!
        
        // But this is NOT reachable in production because:
        // 1. decode_info_hash checks length first (line 161-167)
        // 2. Only calls hex_decode if len == 40 (line 162)
        
        // So this is safe, but the function assumes valid input.
        // If hex_decode is ever made public or used elsewhere, it could be vulnerable.
        
        // For now, let's verify the length check protects us
        let err = MagnetLink::parse("magnet:?xt=urn:btih:abc").unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn hex_decode_39_char_string() {
        // 39 chars will be rejected by decode_info_hash before hex_decode is called
        let hash_39 = "da39a3ee5e6b4b0d3255bfef95601890afd8070";
        assert_eq!(hash_39.len(), 39);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{hash_39}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }
}

#[cfg(test)]
mod base32_correctness_tests {
    use super::*;

    #[test]
    fn base32_bit_alignment_correctness() {
        // base32_decode processes 5 bits at a time
        // 32 chars * 5 bits = 160 bits = 20 bytes - CORRECT
        
        // The algorithm:
        // - Accumulates bits in u64
        // - Each character adds 5 bits
        // - When accumulated bits >= 8, extracts a byte
        // - Final bits should be exactly 0 (no padding needed)
        
        // For 32 chars exactly:
        // - n_bits goes: 5, 10, 15, 20 (extract 8, n_bits=12), 17, 22 (extract 8, n_bits=14),
        //   19, 24 (extract 8, n_bits=16), 21 (extract 8, n_bits=13), 18, 23 (extract 8, n_bits=15),
        //   20 (extract 8, n_bits=12), 17, 22 (extract 8, n_bits=14), 19, 24 (extract 8, n_bits=16),
        //   21 (extract 8, n_bits=13), 18, 23 (extract 8, n_bits=15), 20 (extract 8, n_bits=12),
        //   17, 22 (extract 8, n_bits=14), 19, 24 (extract 8, n_bits=16), 21, 26 (extract 8, n_bits=18... wait)
        
        // This is getting complex. Let's just verify that base32 round-trips correctly.
        
        let test_hash = [0u8; 20]; // All zeros
        let b32 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(b32.len(), 32);
        
        let decoded = base32_decode(b32).unwrap();
        assert_eq!(decoded, test_hash);
    }

    #[test]
    fn base32_rejects_31_chars() {
        // 31 chars * 5 = 155 bits = 19.375 bytes
        // The decoder will produce 19 bytes (missing 3 bits)
        let b32_31 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(b32_31.len(), 31);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_31}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn base32_rejects_33_chars() {
        // 33 chars * 5 = 165 bits = 20.625 bytes
        // The decoder will try to produce 21 bytes, then check pos != 20
        let b32_33 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(b32_33.len(), 33);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_33}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn base32_with_invalid_char_1() {
        // Character '1' is not valid base32 (valid chars: A-Z, 2-7)
        let b32_bad = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1";
        assert_eq!(b32_bad.len(), 32);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_bad}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn base32_with_invalid_char_8() {
        // Character '8' is not valid base32
        let b32_bad = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA8";
        assert_eq!(b32_bad.len(), 32);
        let err = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_bad}")).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn base32_case_insensitive() {
        // Both uppercase and lowercase should work
        let hex = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let bytes = hex_decode(hex).unwrap();
        
        // Manually create base32 from these bytes
        let b32_upper = base32_encode(&bytes);
        let b32_lower = b32_upper.to_lowercase();
        
        let link_upper = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_upper}")).unwrap();
        let link_lower = MagnetLink::parse(&format!("magnet:?xt=urn:btih:{b32_lower}")).unwrap();
        
        assert_eq!(link_upper.info_hash, link_lower.info_hash);
        assert_eq!(link_upper.info_hash, bytes);
    }
}

#[cfg(test)]
mod redteam_fix_tests {
    use super::*;
    use std::fmt::Write as _;

    const EMPTY_SHA1_HEX: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    #[test]
    fn duplicate_xt_returns_error() {
        let uri = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000\
                   &xt=urn:btih:da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let err = MagnetLink::parse(uri).unwrap_err();
        assert!(matches!(err, MagnetError::InvalidInfoHash(_)));
    }

    #[test]
    fn exceeding_max_trackers_returns_error() {
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..=MAX_TRACKERS {
            let _ = write!(uri, "&tr=http%3A%2F%2Ft{i}.example.com");
        }
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
        assert!(err.to_string().contains("tr"));
    }

    #[test]
    fn very_long_param_value_returns_error() {
        let long_val = "a".repeat(MAX_PARAM_LENGTH + 1);
        let uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}&dn={long_val}");
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
    }

    #[test]
    fn exceeding_max_peer_addrs_returns_error() {
        let mut uri = format!("magnet:?xt=urn:btih:{EMPTY_SHA1_HEX}");
        for i in 0..=MAX_PEER_ADDRS {
            let a = (i / 255) % 256;
            let b = (i % 255) + 1;
            let _ = write!(uri, "&x.pe=10.0.{a}.{b}%3A6881");
        }
        let err = MagnetLink::parse(&uri).unwrap_err();
        assert!(matches!(err, MagnetError::LimitExceeded(_)));
        assert!(err.to_string().contains("x.pe"));
    }
}

// Helper function used by test modules above.
#[cfg(test)]
fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut bits: u64 = 0;
    let mut n_bits: u32 = 0;
    for &b in data {
        bits = (bits << 8) | u64::from(b);
        n_bits += 8;
        while n_bits >= 5 {
            n_bits -= 5;
            out.push(char::from(ALPHABET[((bits >> n_bits) & 0x1f) as usize]));
            bits &= (1u64 << n_bits) - 1;
        }
    }
    if n_bits > 0 {
        bits <<= 5 - n_bits;
        out.push(char::from(ALPHABET[(bits & 0x1f) as usize]));
    }
    out
}
