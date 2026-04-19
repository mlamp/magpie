//! BEP 11 Peer Exchange (PEX) message codec.
//!
//! PEX messages are bencoded dictionaries exchanged inside a BEP 10
//! `Extended` envelope with the negotiated `ut_pex` extension id.
//! This module handles only the message encoding/decoding layer; scheduling
//! and session integration live elsewhere.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use magpie_bt_bencode::{Value, decode, encode};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum peers per PEX message to prevent denial-of-service.
pub const MAX_PEX_PEERS: usize = 200;

const V4_ENTRY: usize = 6;
const V6_ENTRY: usize = 18;

// ---------------------------------------------------------------------------
// PexFlags
// ---------------------------------------------------------------------------

/// Per-peer flags in a PEX message (BEP 11 `added.f` / `added6.f` byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PexFlags(pub u8);

impl PexFlags {
    /// Peer prefers encrypted connections.
    pub const PREFERS_ENCRYPTION: u8 = 0x01;
    /// Peer is a seed (upload-only).
    pub const SEED: u8 = 0x02;
    /// Peer supports uTP.
    pub const SUPPORTS_UTP: u8 = 0x04;
    /// Peer supports holepunch.
    pub const SUPPORTS_HOLEPUNCH: u8 = 0x08;
    /// Peer is reachable (connectable).
    pub const REACHABLE: u8 = 0x10;

    /// Returns `true` if the peer prefers encryption.
    #[must_use]
    pub const fn prefers_encryption(self) -> bool {
        self.0 & Self::PREFERS_ENCRYPTION != 0
    }

    /// Returns `true` if the peer is a seed.
    #[must_use]
    pub const fn is_seed(self) -> bool {
        self.0 & Self::SEED != 0
    }

    /// Returns `true` if the peer supports uTP.
    #[must_use]
    pub const fn supports_utp(self) -> bool {
        self.0 & Self::SUPPORTS_UTP != 0
    }

    /// Returns `true` if the peer supports holepunch.
    #[must_use]
    pub const fn supports_holepunch(self) -> bool {
        self.0 & Self::SUPPORTS_HOLEPUNCH != 0
    }

    /// Returns `true` if the peer is reachable.
    #[must_use]
    pub const fn is_reachable(self) -> bool {
        self.0 & Self::REACHABLE != 0
    }
}

// ---------------------------------------------------------------------------
// PexPeer
// ---------------------------------------------------------------------------

/// A peer entry in a PEX message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PexPeer {
    /// Socket address (IPv4 or IPv6).
    pub addr: SocketAddr,
    /// BEP 11 flags for this peer.
    pub flags: PexFlags,
}

// ---------------------------------------------------------------------------
// PexError
// ---------------------------------------------------------------------------

/// Errors that can occur when decoding a PEX message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PexError {
    /// The bencode payload could not be parsed.
    #[error("bencode decode error: {0}")]
    Decode(String),
    /// A compact peer list had a length that is not a multiple of the per-entry stride.
    #[error("compact peer list length {0} not a multiple of {1}")]
    InvalidCompactLength(usize, usize),
    /// The number of peers exceeded the per-message safety cap.
    #[error("too many peers: {0} (max {MAX_PEX_PEERS})")]
    TooManyPeers(usize),
}

// ---------------------------------------------------------------------------
// PexMessage
// ---------------------------------------------------------------------------

/// BEP 11 Peer Exchange message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PexMessage {
    /// Newly connected peers (IPv4 + IPv6 combined).
    pub added: Vec<PexPeer>,
    /// Recently disconnected peers (IPv4 + IPv6 combined).
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    /// Decode a PEX message from a raw bencode payload.
    ///
    /// # Errors
    ///
    /// Returns [`PexError`] on malformed bencode, invalid compact lengths,
    /// or if the peer count exceeds [`MAX_PEX_PEERS`].
    pub fn decode(payload: &[u8]) -> Result<Self, PexError> {
        let value = decode(payload).map_err(|e| PexError::Decode(e.to_string()))?;
        let dict = value
            .as_dict()
            .ok_or_else(|| PexError::Decode("top-level value is not a dict".into()))?;

        // Helper: extract an optional byte-string field.
        let get_bytes = |key: &[u8]| -> &[u8] {
            dict.get(key)
                .and_then(Value::as_bytes)
                .unwrap_or_default()
        };

        // -- added --
        let added_v4_bytes = get_bytes(b"added");
        let added_f_bytes = get_bytes(b"added.f");
        let added_v4 = decode_compact_v4(added_v4_bytes)?;

        // Validate flags length if present and non-empty.
        if !added_f_bytes.is_empty() && added_f_bytes.len() != added_v4.len() {
            return Err(PexError::Decode(
                "added.f length doesn't match added peer count".into(),
            ));
        }
        let added_v4_flags = parse_flags(added_f_bytes, added_v4.len());

        let added_v6_bytes = get_bytes(b"added6");
        let added_v6_flag_bytes = get_bytes(b"added6.f");
        let added6 = decode_compact_v6(added_v6_bytes)?;

        // Validate flags length if present and non-empty.
        if !added_v6_flag_bytes.is_empty() && added_v6_flag_bytes.len() != added6.len() {
            return Err(PexError::Decode(
                "added6.f length doesn't match added6 peer count".into(),
            ));
        }
        let added6_flags = parse_flags(added_v6_flag_bytes, added6.len());

        // Check combined peer count before allocating the merged list.
        let total_added = added_v4.len() + added6.len();
        if total_added > MAX_PEX_PEERS {
            return Err(PexError::TooManyPeers(total_added));
        }

        let mut added: Vec<PexPeer> = Vec::with_capacity(total_added);
        for (addr, flags) in added_v4.into_iter().zip(added_v4_flags) {
            added.push(PexPeer { addr, flags });
        }
        for (addr, flags) in added6.into_iter().zip(added6_flags) {
            added.push(PexPeer { addr, flags });
        }

        // -- dropped --
        let dropped_v4_bytes = get_bytes(b"dropped");
        let dropped6_bytes = get_bytes(b"dropped6");
        let mut dropped = decode_compact_v4(dropped_v4_bytes)?;
        dropped.extend(decode_compact_v6(dropped6_bytes)?);

        if dropped.len() > MAX_PEX_PEERS {
            return Err(PexError::TooManyPeers(dropped.len()));
        }

        Ok(Self { added, dropped })
    }

    /// Encode this PEX message into a bencode byte vector.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();

        // Split added into v4 / v6.
        let (added_v4, added_v6): (Vec<_>, Vec<_>) = self
            .added
            .iter()
            .partition(|p| matches!(p.addr, SocketAddr::V4(_)));

        let v4_addrs: Vec<SocketAddr> = added_v4.iter().map(|p| p.addr).collect();
        let v4_flags: Vec<u8> = added_v4.iter().map(|p| p.flags.0).collect();
        let v6_addrs: Vec<SocketAddr> = added_v6.iter().map(|p| p.addr).collect();
        let v6_flags: Vec<u8> = added_v6.iter().map(|p| p.flags.0).collect();

        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v4(&v4_addrs))),
        );
        dict.insert(
            Cow::Borrowed(b"added.f".as_slice()),
            Value::Bytes(Cow::Owned(v4_flags)),
        );
        dict.insert(
            Cow::Borrowed(b"added6".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v6(&v6_addrs))),
        );
        dict.insert(
            Cow::Borrowed(b"added6.f".as_slice()),
            Value::Bytes(Cow::Owned(v6_flags)),
        );

        // Split dropped into v4 / v6.
        let (dropped_v4, dropped_v6): (Vec<_>, Vec<_>) = self
            .dropped
            .iter()
            .partition(|a| matches!(a, SocketAddr::V4(_)));

        dict.insert(
            Cow::Borrowed(b"dropped".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v4(&dropped_v4))),
        );
        dict.insert(
            Cow::Borrowed(b"dropped6".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v6(&dropped_v6))),
        );

        encode(&Value::Dict(dict))
    }
}

// ---------------------------------------------------------------------------
// Compact peer helpers (private)
// ---------------------------------------------------------------------------

/// Parse flags bytes, padding with `PexFlags::default()` if shorter than `count`.
fn parse_flags(bytes: &[u8], count: usize) -> Vec<PexFlags> {
    (0..count)
        .map(|i| PexFlags(bytes.get(i).copied().unwrap_or(0)))
        .collect()
}

fn encode_compact_v4(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addrs.len() * V4_ENTRY);
    for addr in addrs {
        if let IpAddr::V4(ip) = addr.ip() {
            out.extend_from_slice(&ip.octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
    }
    out
}

fn decode_compact_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>, PexError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.len().is_multiple_of(V4_ENTRY) {
        return Err(PexError::InvalidCompactLength(bytes.len(), V4_ENTRY));
    }
    let mut out = Vec::with_capacity(bytes.len() / V4_ENTRY);
    for chunk in bytes.chunks_exact(V4_ENTRY) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        out.push(SocketAddr::new(IpAddr::V4(ip), port));
    }
    Ok(out)
}

fn encode_compact_v6(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addrs.len() * V6_ENTRY);
    for addr in addrs {
        if let IpAddr::V6(ip) = addr.ip() {
            out.extend_from_slice(&ip.octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
    }
    out
}

fn decode_compact_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>, PexError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.len().is_multiple_of(V6_ENTRY) {
        return Err(PexError::InvalidCompactLength(bytes.len(), V6_ENTRY));
    }
    let mut out = Vec::with_capacity(bytes.len() / V6_ENTRY);
    for chunk in bytes.chunks_exact(V6_ENTRY) {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        out.push(SocketAddr::new(IpAddr::V6(ip), port));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- PexFlags tests -----------------------------------------------------

    #[test]
    fn flags_default_is_zero() {
        let f = PexFlags::default();
        assert_eq!(f.0, 0);
        assert!(!f.is_seed());
        assert!(!f.is_reachable());
        assert!(!f.prefers_encryption());
        assert!(!f.supports_utp());
        assert!(!f.supports_holepunch());
    }

    #[test]
    fn flags_individual_bits() {
        let f = PexFlags(PexFlags::SEED | PexFlags::SUPPORTS_UTP);
        assert!(f.is_seed());
        assert!(f.supports_utp());
        assert!(!f.prefers_encryption());
        assert!(!f.is_reachable());
        assert!(!f.supports_holepunch());
    }

    #[test]
    fn flags_all_bits() {
        let f = PexFlags(0x1F);
        assert!(f.prefers_encryption());
        assert!(f.is_seed());
        assert!(f.supports_utp());
        assert!(f.supports_holepunch());
        assert!(f.is_reachable());
    }

    // -- Compact peer helpers -----------------------------------------------

    #[test]
    fn compact_v4_roundtrip() {
        let addrs = vec![
            "10.0.0.1:6881".parse::<SocketAddr>().unwrap(),
            "192.168.1.2:49205".parse().unwrap(),
        ];
        let encoded = encode_compact_v4(&addrs);
        assert_eq!(encoded.len(), 12);
        let decoded = decode_compact_v4(&encoded).unwrap();
        assert_eq!(decoded, addrs);
    }

    #[test]
    fn compact_v6_roundtrip() {
        let addrs = vec!["[::1]:6881".parse::<SocketAddr>().unwrap()];
        let encoded = encode_compact_v6(&addrs);
        assert_eq!(encoded.len(), 18);
        let decoded = decode_compact_v6(&encoded).unwrap();
        assert_eq!(decoded, addrs);
    }

    #[test]
    fn compact_v4_invalid_length() {
        let err = decode_compact_v4(&[1, 2, 3, 4, 5]).unwrap_err();
        assert!(matches!(err, PexError::InvalidCompactLength(5, 6)));
    }

    #[test]
    fn compact_v6_invalid_length() {
        let err = decode_compact_v6(&[0u8; 17]).unwrap_err();
        assert!(matches!(err, PexError::InvalidCompactLength(17, 18)));
    }

    #[test]
    fn compact_empty_is_ok() {
        assert_eq!(decode_compact_v4(&[]).unwrap(), Vec::<SocketAddr>::new());
        assert_eq!(decode_compact_v6(&[]).unwrap(), Vec::<SocketAddr>::new());
    }

    // -- PexMessage encode/decode -------------------------------------------

    fn sample_v4_peer(port: u16, flags: u8) -> PexPeer {
        PexPeer {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port),
            flags: PexFlags(flags),
        }
    }

    fn sample_v6_peer(port: u16, flags: u8) -> PexPeer {
        PexPeer {
            addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
            flags: PexFlags(flags),
        }
    }

    #[test]
    fn roundtrip_ipv4_only() {
        let msg = PexMessage {
            added: vec![
                sample_v4_peer(6881, PexFlags::SEED),
                sample_v4_peer(6882, PexFlags::REACHABLE),
            ],
            dropped: vec!["10.0.0.2:6883".parse().unwrap()],
        };
        let encoded = msg.encode();
        let decoded = PexMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn roundtrip_ipv6_only() {
        let msg = PexMessage {
            added: vec![sample_v6_peer(6881, PexFlags::SUPPORTS_UTP)],
            dropped: vec!["[::1]:6882".parse().unwrap()],
        };
        let encoded = msg.encode();
        let decoded = PexMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn roundtrip_mixed_v4_v6() {
        let msg = PexMessage {
            added: vec![
                sample_v4_peer(6881, PexFlags::SEED | PexFlags::PREFERS_ENCRYPTION),
                sample_v6_peer(6882, PexFlags::SUPPORTS_HOLEPUNCH),
            ],
            dropped: vec![
                "10.0.0.3:6883".parse().unwrap(),
                "[::1]:6884".parse().unwrap(),
            ],
        };
        let encoded = msg.encode();
        let decoded = PexMessage::decode(&encoded).unwrap();
        // IPv4 added come before IPv6 added in the decoded output (they are
        // reconstructed from separate fields), matching the encode order.
        assert_eq!(decoded, msg);
    }

    #[test]
    fn empty_pex_message() {
        let msg = PexMessage {
            added: vec![],
            dropped: vec![],
        };
        let encoded = msg.encode();
        let decoded = PexMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_empty_dict() {
        // Minimal valid bencode dict: "de"
        let decoded = PexMessage::decode(b"de").unwrap();
        assert!(decoded.added.is_empty());
        assert!(decoded.dropped.is_empty());
    }

    #[test]
    fn flags_roundtrip_in_message() {
        let msg = PexMessage {
            added: vec![PexPeer {
                addr: "10.0.0.1:6881".parse().unwrap(),
                flags: PexFlags(0x1F), // all flags set
            }],
            dropped: vec![],
        };
        let encoded = msg.encode();
        let decoded = PexMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.added[0].flags, PexFlags(0x1F));
    }

    #[test]
    fn decode_rejects_invalid_bencode() {
        let err = PexMessage::decode(b"not bencode").unwrap_err();
        assert!(matches!(err, PexError::Decode(_)));
    }

    #[test]
    fn decode_rejects_non_dict() {
        // A bencode integer instead of dict.
        let err = PexMessage::decode(b"i42e").unwrap_err();
        assert!(matches!(err, PexError::Decode(_)));
    }

    #[test]
    fn decode_rejects_truncated_compact_v4() {
        // Build a dict with an `added` field whose length is not a multiple of 6.
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Borrowed(&[1, 2, 3, 4, 5])),
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::InvalidCompactLength(5, 6)));
    }

    #[test]
    fn decode_rejects_truncated_compact_v6() {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"added6".as_slice()),
            Value::Bytes(Cow::Borrowed(&[0u8; 17])),
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::InvalidCompactLength(17, 18)));
    }

    #[test]
    fn decode_rejects_too_many_added_peers() {
        // Build a compact v4 list with MAX_PEX_PEERS + 1 entries.
        let count = MAX_PEX_PEERS + 1;
        let mut compact = Vec::with_capacity(count * V4_ENTRY);
        for i in 0..count {
            compact.extend_from_slice(&Ipv4Addr::new(10, 0, #[allow(clippy::cast_possible_truncation)] { (i >> 8) as u8 }, #[allow(clippy::cast_possible_truncation)] { i as u8 }).octets());
            compact.extend_from_slice(&6881u16.to_be_bytes());
        }
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(compact)),
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::TooManyPeers(_)));
    }

    #[test]
    fn decode_rejects_too_many_dropped_peers() {
        let count = MAX_PEX_PEERS + 1;
        let mut compact = Vec::with_capacity(count * V4_ENTRY);
        for i in 0..count {
            compact.extend_from_slice(&Ipv4Addr::new(10, 0, #[allow(clippy::cast_possible_truncation)] { (i >> 8) as u8 }, #[allow(clippy::cast_possible_truncation)] { i as u8 }).octets());
            compact.extend_from_slice(&6881u16.to_be_bytes());
        }
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"dropped".as_slice()),
            Value::Bytes(Cow::Owned(compact)),
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::TooManyPeers(_)));
    }

    #[test]
    fn decode_rejects_fewer_flags_than_peers() {
        // 2 IPv4 peers but only 1 flag byte.
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        let addrs: Vec<SocketAddr> = vec![
            "10.0.0.1:6881".parse().unwrap(),
            "10.0.0.2:6882".parse().unwrap(),
        ];
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v4(&addrs))),
        );
        dict.insert(
            Cow::Borrowed(b"added.f".as_slice()),
            Value::Bytes(Cow::Borrowed(&[0x02])), // only 1 flag for 2 peers
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::Decode(ref msg) if msg.contains("added.f")));
    }

    #[test]
    fn decode_rejects_more_flags_than_peers() {
        // 1 IPv4 peer but 3 flag bytes.
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        let addrs: Vec<SocketAddr> = vec!["10.0.0.1:6881".parse().unwrap()];
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(encode_compact_v4(&addrs))),
        );
        dict.insert(
            Cow::Borrowed(b"added.f".as_slice()),
            Value::Bytes(Cow::Borrowed(&[0x02, 0x04, 0x08])), // 3 flags for 1 peer
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::Decode(ref msg) if msg.contains("added.f")));
    }

    #[test]
    fn decode_rejects_combined_v4_v6_exceeding_max() {
        // Put MAX_PEX_PEERS v4 peers + 1 v6 peer to exceed the limit.
        let v4_count = MAX_PEX_PEERS;
        let mut compact_v4 = Vec::with_capacity(v4_count * V4_ENTRY);
        for i in 0..v4_count {
            compact_v4
                .extend_from_slice(&Ipv4Addr::new(10, 0, #[allow(clippy::cast_possible_truncation)] { (i >> 8) as u8 }, #[allow(clippy::cast_possible_truncation)] { i as u8 }).octets());
            compact_v4.extend_from_slice(&6881u16.to_be_bytes());
        }
        let compact_v6 = encode_compact_v6(&["[::1]:6881".parse().unwrap()]);

        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(compact_v4)),
        );
        dict.insert(
            Cow::Borrowed(b"added6".as_slice()),
            Value::Bytes(Cow::Owned(compact_v6)),
        );
        let payload = encode(&Value::Dict(dict));
        let err = PexMessage::decode(&payload).unwrap_err();
        assert!(matches!(err, PexError::TooManyPeers(_)));
    }

    #[test]
    fn missing_flags_defaults_to_zero() {
        // Encode a message with `added` but no `added.f`.
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        let compact = encode_compact_v4(&["10.0.0.1:6881".parse().unwrap()]);
        dict.insert(
            Cow::Borrowed(b"added".as_slice()),
            Value::Bytes(Cow::Owned(compact)),
        );
        let payload = encode(&Value::Dict(dict));
        let decoded = PexMessage::decode(&payload).unwrap();
        assert_eq!(decoded.added.len(), 1);
        assert_eq!(decoded.added[0].flags, PexFlags(0));
    }
}
