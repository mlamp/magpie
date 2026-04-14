//! BEP 23 (compact tracker response) and BEP 7 (IPv6 compact response) decoders.
//!
//! Compact peer entries are fixed-size big-endian byte tuples:
//!
//! - IPv4: `[octets; 4][port; 2]` → 6 bytes per peer.
//! - IPv6: `[octets; 16][port; 2]` → 18 bytes per peer.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::tracker::error::TrackerError;

const V4_ENTRY: usize = 6;
const V6_ENTRY: usize = 18;

/// Decode a compact IPv4 peer list (BEP 23).
///
/// # Errors
///
/// Returns [`TrackerError::CompactPeersTruncated`] if the input length is not a
/// multiple of 6.
pub(super) fn decode_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    decode(bytes, V4_ENTRY, |entry| {
        let ip = Ipv4Addr::new(entry[0], entry[1], entry[2], entry[3]);
        let port = u16::from_be_bytes([entry[4], entry[5]]);
        SocketAddr::new(IpAddr::V4(ip), port)
    })
}

/// Decode a compact IPv6 peer list (BEP 7).
///
/// # Errors
///
/// Returns [`TrackerError::CompactPeersTruncated`] if the input length is not a
/// multiple of 18.
pub(super) fn decode_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    decode(bytes, V6_ENTRY, |entry| {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&entry[..16]);
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([entry[16], entry[17]]);
        SocketAddr::new(IpAddr::V6(ip), port)
    })
}

fn decode<F>(bytes: &[u8], stride: usize, mk: F) -> Result<Vec<SocketAddr>, TrackerError>
where
    F: Fn(&[u8]) -> SocketAddr,
{
    if !bytes.len().is_multiple_of(stride) {
        return Err(TrackerError::CompactPeersTruncated(bytes.len(), stride));
    }
    let mut out = Vec::with_capacity(bytes.len() / stride);
    for chunk in bytes.chunks_exact(stride) {
        let addr = mk(chunk);
        if addr.port() == 0 {
            // BEP 23 reserves port 0 to mean "no peer"; rasterbar / librqbit drop these.
            continue;
        }
        out.push(addr);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_v4_two_peers() {
        let bytes = [10, 0, 0, 1, 0x1A, 0xE1, 192, 168, 1, 2, 0xC0, 0x35];
        let peers = decode_v4(&bytes).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].to_string(), "10.0.0.1:6881");
        assert_eq!(peers[1].to_string(), "192.168.1.2:49205");
    }

    #[test]
    fn decode_v4_drops_zero_port() {
        let bytes = [10, 0, 0, 1, 0, 0, 10, 0, 0, 2, 0x1A, 0xE1];
        let peers = decode_v4(&bytes).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "10.0.0.2:6881");
    }

    #[test]
    fn decode_v4_rejects_truncated() {
        let bytes = [1, 2, 3, 4, 5];
        assert!(matches!(
            decode_v4(&bytes),
            Err(TrackerError::CompactPeersTruncated(5, 6))
        ));
    }

    #[test]
    fn decode_v6_one_peer() {
        let mut bytes = vec![0u8; 18];
        bytes[15] = 1; // ::1
        bytes[16] = 0x1A;
        bytes[17] = 0xE1;
        let peers = decode_v6(&bytes).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "[::1]:6881");
    }

    #[test]
    fn decode_v6_rejects_truncated() {
        let bytes = [0u8; 17];
        assert!(matches!(
            decode_v6(&bytes),
            Err(TrackerError::CompactPeersTruncated(17, 18))
        ));
    }
}
