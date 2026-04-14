//! BEP 15 UDP tracker codec + primitives.
//!
//! Protocol shape:
//!
//! ```text
//! CONNECT req   16B: protocol_id u64 | action=0 u32 | transaction_id u32
//! CONNECT resp  16B: action=0 u32 | transaction_id u32 | connection_id u64
//!
//! ANNOUNCE req  98B: connection_id u64 | action=1 u32 | transaction_id u32
//!                 | info_hash [u8;20] | peer_id [u8;20]
//!                 | downloaded u64 | left u64 | uploaded u64
//!                 | event u32 | ip u32 | key u32
//!                 | num_want i32 | port u16
//! ANNOUNCE resp 20+N*6 B: action=1 u32 | transaction_id u32
//!                      | interval u32 | leechers u32 | seeders u32
//!                      | (peer_ip u32, peer_port u16)*
//! ```
//!
//! Connection IDs live **60 s**; the client refreshes on expiry per ADR-0015
//! / milestone plan invariant #10.
//!
//! This module ships the **codec only** for this iteration; the high-level
//! client that drives connect→announce, backoff, and retries is in a
//! follow-up commit wired to [`crate::session::udp::demux::UdpDemux`].

#![allow(clippy::cast_possible_truncation)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use super::{AnnounceEvent, AnnounceRequest, AnnounceResponse, TrackerError};

/// BEP 15 magic constant. First 8 bytes of any `CONNECT` request.
pub const PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;

/// BEP 15 `action` discriminant: CONNECT.
pub const ACTION_CONNECT: u32 = 0;
/// BEP 15 `action` discriminant: ANNOUNCE.
pub const ACTION_ANNOUNCE: u32 = 1;
/// BEP 15 `action` discriminant: SCRAPE.
pub const ACTION_SCRAPE: u32 = 2;
/// BEP 15 `action` discriminant: ERROR (tracker returned a failure reason).
pub const ACTION_ERROR: u32 = 3;

/// Connection-id validity window (BEP 15).
pub const CONNECTION_ID_TTL: Duration = Duration::from_secs(60);

/// Encode a CONNECT request.
#[must_use]
pub fn encode_connect(transaction_id: u32) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    buf
}

/// Decode a CONNECT response. Returns `connection_id` on success.
///
/// # Errors
///
/// [`TrackerError::MalformedResponse`] on bad length, action mismatch, or
/// transaction-id mismatch.
pub fn decode_connect(bytes: &[u8], expected_txid: u32) -> Result<u64, TrackerError> {
    if bytes.len() < 16 {
        return Err(TrackerError::MalformedResponse(
            "connect response < 16 bytes".into(),
        ));
    }
    let action = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&bytes[8..]).into_owned();
        return Err(TrackerError::Failure(msg));
    }
    if action != ACTION_CONNECT {
        return Err(TrackerError::MalformedResponse(format!(
            "unexpected action {action}"
        )));
    }
    let txid = u32::from_be_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
    if txid != expected_txid {
        return Err(TrackerError::MalformedResponse(
            "transaction_id mismatch".into(),
        ));
    }
    let conn_id = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
    Ok(conn_id)
}

/// Encode an ANNOUNCE request. `key` is a random u32 the tracker uses to
/// identify us across NAT rebinds — the caller should generate one per
/// session and reuse.
#[must_use]
pub fn encode_announce(
    connection_id: u64,
    transaction_id: u32,
    req: &AnnounceRequest<'_>,
    key: u32,
) -> [u8; 98] {
    let mut buf = [0u8; 98];
    buf[0..8].copy_from_slice(&connection_id.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    buf[16..36].copy_from_slice(&req.info_hash);
    buf[36..56].copy_from_slice(&req.peer_id);
    buf[56..64].copy_from_slice(&req.downloaded.to_be_bytes());
    buf[64..72].copy_from_slice(&req.left.to_be_bytes());
    buf[72..80].copy_from_slice(&req.uploaded.to_be_bytes());
    let event: u32 = match req.event {
        AnnounceEvent::Periodic => 0,
        AnnounceEvent::Completed => 1,
        AnnounceEvent::Started => 2,
        AnnounceEvent::Stopped => 3,
    };
    buf[80..84].copy_from_slice(&event.to_be_bytes());
    buf[84..88].copy_from_slice(&[0u8; 4]); // ip=0 → tracker infers
    buf[88..92].copy_from_slice(&key.to_be_bytes());
    let num_want = req
        .num_want
        .map_or(-1i32, |v| v.try_into().unwrap_or(i32::MAX));
    buf[92..96].copy_from_slice(&num_want.to_be_bytes());
    buf[96..98].copy_from_slice(&req.port.to_be_bytes());
    buf
}

/// Decode an ANNOUNCE response. Peer list comes out as IPv4 socket addrs
/// (BEP 15 is IPv4-only; v6 uses a separate protocol variant).
///
/// # Errors
///
/// [`TrackerError::MalformedResponse`] on length/action/transaction-id mismatch.
/// [`TrackerError::Failure`] if the tracker replied with `action=3` (error).
pub fn decode_announce(bytes: &[u8], expected_txid: u32) -> Result<AnnounceResponse, TrackerError> {
    if bytes.len() < 8 {
        return Err(TrackerError::MalformedResponse(
            "announce response < 8 bytes".into(),
        ));
    }
    let action = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    let txid = u32::from_be_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
    if txid != expected_txid {
        return Err(TrackerError::MalformedResponse(
            "transaction_id mismatch".into(),
        ));
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&bytes[8..]).into_owned();
        return Err(TrackerError::Failure(msg));
    }
    if action != ACTION_ANNOUNCE {
        return Err(TrackerError::MalformedResponse(format!(
            "unexpected action {action}"
        )));
    }
    if bytes.len() < 20 {
        return Err(TrackerError::MalformedResponse(
            "announce response < 20 bytes".into(),
        ));
    }
    let interval = u32::from_be_bytes(bytes[8..12].try_into().unwrap_or([0; 4]));
    let leechers = u32::from_be_bytes(bytes[12..16].try_into().unwrap_or([0; 4]));
    let seeders = u32::from_be_bytes(bytes[16..20].try_into().unwrap_or([0; 4]));
    let peer_bytes = &bytes[20..];
    if !peer_bytes.len().is_multiple_of(6) {
        return Err(TrackerError::MalformedResponse(
            "peer list length is not a multiple of 6".into(),
        ));
    }
    let mut peers = Vec::with_capacity(peer_bytes.len() / 6);
    for chunk in peer_bytes.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        peers.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    Ok(AnnounceResponse {
        interval: Duration::from_secs(u64::from(interval)),
        min_interval: None,
        peers,
        tracker_id: None,
        complete: Some(seeders),
        incomplete: Some(leechers),
        warning: None,
    })
}

/// BEP 15 retry timing (spec §5): request timeout is `15 * 2^n` seconds,
/// n starting at 0, capped at 3840 s.
#[must_use]
pub const fn retry_timeout(attempt: u32) -> Duration {
    let shift = if attempt > 8 { 8 } else { attempt };
    let secs = 15u64 << shift;
    let capped = if secs > 3840 { 3840 } else { secs };
    Duration::from_secs(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_request_has_protocol_id_first() {
        let buf = encode_connect(0xDEAD_BEEF);
        assert_eq!(&buf[0..8], &PROTOCOL_ID.to_be_bytes());
        assert_eq!(&buf[8..12], &ACTION_CONNECT.to_be_bytes());
        assert_eq!(&buf[12..16], &0xDEAD_BEEFu32.to_be_bytes());
    }

    #[test]
    fn decode_connect_roundtrip() {
        let mut resp = [0u8; 16];
        resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        resp[4..8].copy_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        resp[8..16].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_be_bytes());
        let conn_id = decode_connect(&resp, 0xCAFE_BABE).unwrap();
        assert_eq!(conn_id, 0x1234_5678_9ABC_DEF0);
    }

    #[test]
    fn decode_connect_rejects_wrong_txid() {
        let mut resp = [0u8; 16];
        resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        resp[4..8].copy_from_slice(&1u32.to_be_bytes());
        let err = decode_connect(&resp, 2).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn decode_connect_propagates_tracker_error() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        resp.extend_from_slice(&42u32.to_be_bytes());
        resp.extend_from_slice(b"torrent unknown");
        let err = decode_connect(&resp, 42).unwrap_err();
        match err {
            TrackerError::Failure(msg) => assert_eq!(msg, "torrent unknown"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn announce_request_layout() {
        let req = AnnounceRequest {
            info_hash: [0xAA; 20],
            peer_id: [0xBB; 20],
            port: 6881,
            uploaded: 1000,
            downloaded: 500,
            left: 2000,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
            tracker_id: None,
        };
        let buf = encode_announce(0x1111_2222_3333_4444, 0x5555_6666, &req, 0x7777_8888);
        assert_eq!(&buf[0..8], &0x1111_2222_3333_4444u64.to_be_bytes());
        assert_eq!(&buf[8..12], &ACTION_ANNOUNCE.to_be_bytes());
        assert_eq!(&buf[12..16], &0x5555_6666u32.to_be_bytes());
        assert_eq!(&buf[16..36], &[0xAAu8; 20]);
        assert_eq!(&buf[36..56], &[0xBBu8; 20]);
        assert_eq!(&buf[56..64], &500u64.to_be_bytes());
        assert_eq!(&buf[64..72], &2000u64.to_be_bytes());
        assert_eq!(&buf[72..80], &1000u64.to_be_bytes());
        assert_eq!(&buf[80..84], &2u32.to_be_bytes()); // started
        assert_eq!(&buf[92..96], &50i32.to_be_bytes());
        assert_eq!(&buf[96..98], &6881u16.to_be_bytes());
    }

    #[test]
    fn decode_announce_parses_peer_list() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        resp.extend_from_slice(&0xAAAA_BBBBu32.to_be_bytes());
        resp.extend_from_slice(&1800u32.to_be_bytes()); // interval
        resp.extend_from_slice(&10u32.to_be_bytes()); // leechers
        resp.extend_from_slice(&20u32.to_be_bytes()); // seeders
        resp.extend_from_slice(&[192, 168, 1, 5, 0x1A, 0xE1]); // 192.168.1.5:6881
        resp.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE1]); // 10.0.0.1:6881
        let r = decode_announce(&resp, 0xAAAA_BBBB).unwrap();
        assert_eq!(r.interval, Duration::from_secs(1800));
        assert_eq!(r.complete, Some(20));
        assert_eq!(r.incomplete, Some(10));
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.peers[0].port(), 6881);
    }

    #[test]
    fn retry_timeout_follows_bep15_curve() {
        assert_eq!(retry_timeout(0), Duration::from_secs(15));
        assert_eq!(retry_timeout(1), Duration::from_secs(30));
        assert_eq!(retry_timeout(2), Duration::from_secs(60));
        assert_eq!(retry_timeout(7), Duration::from_secs(1920));
        assert_eq!(retry_timeout(8), Duration::from_secs(3840)); // cap
        assert_eq!(retry_timeout(20), Duration::from_secs(3840));
    }
}
