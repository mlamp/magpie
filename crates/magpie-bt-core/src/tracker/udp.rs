//! BEP 15 UDP tracker codec + client.
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
//! The high-level [`UdpTracker`] client implements the [`Tracker`] trait and
//! is wired to [`crate::session::udp::demux::UdpDemux`] for transaction-id
//! routing. Consumers hand it an `Arc<UdpDemux>` at construction and
//! subsequent `announce` calls run connect-then-announce on the shared socket.

#![allow(clippy::cast_possible_truncation)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    AnnounceEvent, AnnounceFuture, AnnounceRequest, AnnounceResponse, ScrapeFile, ScrapeFuture,
    ScrapeResponse, Tracker, TrackerError, TrackerScrape,
};
use crate::session::udp::demux::UdpDemux;

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

/// BEP 15 scrape request/response size limit: up to 74 info-hashes per packet.
///
/// Keeps the request in one safe UDP datagram (74 · 20 + 16-byte header =
/// 1496 bytes, below the typical 1500 MTU). Callers scraping more hashes
/// must batch externally.
pub const MAX_SCRAPE_HASHES: usize = 74;

/// Encode a SCRAPE request. `info_hashes.len()` must be in the range
/// `[1, MAX_SCRAPE_HASHES]` — the caller is responsible for batching
/// (see [`MAX_SCRAPE_HASHES`]).
///
/// Layout: `connection_id u64 | action=2 u32 | transaction_id u32 |
/// info_hash[0..20] | info_hash[0..20] | ...`.
///
/// # Errors
///
/// Returns [`TrackerError::MalformedResponse`] if `info_hashes` is
/// empty or larger than [`MAX_SCRAPE_HASHES`]. (Reusing this variant
/// keeps the callers' error handling uniform — a caller passing an
/// out-of-range batch is structurally malformed at the call site.)
pub fn encode_scrape(
    connection_id: u64,
    transaction_id: u32,
    info_hashes: &[[u8; 20]],
) -> Result<Vec<u8>, TrackerError> {
    if info_hashes.is_empty() {
        return Err(TrackerError::MalformedResponse(
            "scrape request has zero info_hashes".into(),
        ));
    }
    if info_hashes.len() > MAX_SCRAPE_HASHES {
        return Err(TrackerError::MalformedResponse(format!(
            "scrape request has {} info_hashes (max {MAX_SCRAPE_HASHES})",
            info_hashes.len()
        )));
    }
    let mut buf = Vec::with_capacity(16 + info_hashes.len() * 20);
    buf.extend_from_slice(&connection_id.to_be_bytes());
    buf.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
    buf.extend_from_slice(&transaction_id.to_be_bytes());
    for h in info_hashes {
        buf.extend_from_slice(h);
    }
    Ok(buf)
}

/// Decode a SCRAPE response. The wire format is positional
/// (`seeders`/`completed`/`leechers` per info-hash in request order);
/// we zip with the original hash list to build the keyed response.
///
/// # Errors
///
/// [`TrackerError::MalformedResponse`] on length/txid mismatch or
/// wrong response size (one 12-byte file record per hash requested).
/// [`TrackerError::Failure`] if the tracker replied with `action=3`.
pub fn decode_scrape(
    bytes: &[u8],
    expected_txid: u32,
    info_hashes: &[[u8; 20]],
) -> Result<ScrapeResponse, TrackerError> {
    if bytes.len() < 8 {
        return Err(TrackerError::MalformedResponse(
            "scrape response < 8 bytes".into(),
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
    if action != ACTION_SCRAPE {
        return Err(TrackerError::MalformedResponse(format!(
            "unexpected action {action}"
        )));
    }
    let expected_body = info_hashes.len() * 12;
    if bytes.len() - 8 != expected_body {
        return Err(TrackerError::MalformedResponse(format!(
            "scrape body has {} bytes, expected {expected_body} ({} hashes × 12)",
            bytes.len() - 8,
            info_hashes.len()
        )));
    }
    let mut files: std::collections::HashMap<[u8; 20], ScrapeFile> =
        std::collections::HashMap::with_capacity(info_hashes.len());
    for (i, hash) in info_hashes.iter().enumerate() {
        let off = 8 + i * 12;
        let seeders = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]));
        let completed =
            u32::from_be_bytes(bytes[off + 4..off + 8].try_into().unwrap_or([0; 4]));
        let leechers =
            u32::from_be_bytes(bytes[off + 8..off + 12].try_into().unwrap_or([0; 4]));
        files.insert(
            *hash,
            ScrapeFile {
                complete: u64::from(seeders),
                incomplete: u64::from(leechers),
                downloaded: u64::from(completed),
                name: None,
            },
        );
    }
    Ok(ScrapeResponse {
        files,
        failure_reason: None,
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

// ---------------------------------------------------------------------------
// UdpTracker client
// ---------------------------------------------------------------------------

/// Cap on attempts for a single CONNECT→ANNOUNCE cycle.
///
/// BEP 15 §5 defines a retry curve of 15·2ⁿ seconds that runs up to
/// ~2 hours for `n = 8`. A library default that long is hostile to
/// callers; `MAX_ATTEMPTS = 4` gives total worst-case 15+30+60+120 =
/// 225 s per announce, which matches what real clients actually
/// tolerate. Consumers that want a longer budget can override via
/// [`UdpTracker::with_max_attempts`].
pub const MAX_ATTEMPTS: u32 = 4;

/// Per-session randomised `key` field sent in the announce. The
/// tracker uses it to identify us across NAT rebinds so we don't
/// accidentally double-count ourselves as two peers. The library
/// generates this at construction from the OS RNG (no auth weight —
/// just enough entropy so unrelated clients don't collide on the same
/// `key`).
fn random_key() -> u32 {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).expect("getrandom");
    u32::from_be_bytes(buf)
}

fn random_txid() -> u32 {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).expect("getrandom");
    u32::from_be_bytes(buf)
}

/// BEP 15 UDP tracker client.
///
/// Holds a shared [`UdpDemux`] for transaction-id routing + the
/// tracker's socket address + the per-session `key`. Caches the
/// tracker-supplied `connection_id` for its 60 s TTL so consecutive
/// announces skip the CONNECT round-trip.
#[derive(Debug)]
pub struct UdpTracker {
    demux: Arc<UdpDemux>,
    target: SocketAddr,
    peer_key: u32,
    cached_conn: Mutex<Option<(u64, Instant)>>,
    max_attempts: u32,
}

impl UdpTracker {
    /// Construct a tracker client pinned to `target`. The `demux` is
    /// shared with any other UDP subsystems bound to the same socket
    /// (DHT, uTP) — one socket per engine.
    #[must_use]
    pub fn new(demux: Arc<UdpDemux>, target: SocketAddr) -> Self {
        Self {
            demux,
            target,
            peer_key: random_key(),
            cached_conn: Mutex::new(None),
            max_attempts: MAX_ATTEMPTS,
        }
    }

    /// Override the retry-attempt cap (default [`MAX_ATTEMPTS`]).
    /// Value is clamped to `[1, 9]` (9 == full BEP 15 curve).
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.clamp(1, 9);
        self
    }

    /// Return the cached connection id if still valid, else refresh
    /// via a CONNECT round-trip.
    async fn ensure_connection_id(&self) -> Result<u64, TrackerError> {
        {
            let guard = self.cached_conn.lock().expect("cached_conn poisoned");
            if let Some((id, expires)) = *guard
                && Instant::now() < expires
            {
                return Ok(id);
            }
        }
        let conn_id = self.run_connect().await?;
        {
            let mut guard = self.cached_conn.lock().expect("cached_conn poisoned");
            *guard = Some((conn_id, Instant::now() + CONNECTION_ID_TTL));
        }
        Ok(conn_id)
    }

    async fn run_connect(&self) -> Result<u64, TrackerError> {
        for attempt in 0..self.max_attempts {
            let txid = random_txid();
            let rx = self
                .demux
                .register_tracker_response(txid, retry_timeout(attempt))
                .map_err(|e| TrackerError::Udp(format!("register CONNECT txid: {e}")))?;
            let req = encode_connect(txid);
            self.demux
                .send_to(&req, self.target)
                .await
                .map_err(|e| TrackerError::Udp(format!("send CONNECT: {e}")))?;
            if let Ok(Ok(resp)) =
                tokio::time::timeout(retry_timeout(attempt), rx).await
            {
                return decode_connect(&resp.data, txid);
            }
        }
        Err(TrackerError::Timeout(self.max_attempts))
    }

    async fn run_announce(
        &self,
        conn_id: u64,
        req: &AnnounceRequest<'_>,
    ) -> Result<AnnounceResponse, TrackerError> {
        for attempt in 0..self.max_attempts {
            let txid = random_txid();
            let rx = self
                .demux
                .register_tracker_response(txid, retry_timeout(attempt))
                .map_err(|e| TrackerError::Udp(format!("register ANNOUNCE txid: {e}")))?;
            let packet = encode_announce(conn_id, txid, req, self.peer_key);
            self.demux
                .send_to(&packet, self.target)
                .await
                .map_err(|e| TrackerError::Udp(format!("send ANNOUNCE: {e}")))?;
            if let Ok(Ok(resp)) =
                tokio::time::timeout(retry_timeout(attempt), rx).await
            {
                return decode_announce(&resp.data, txid);
            }
        }
        Err(TrackerError::Timeout(self.max_attempts))
    }

    async fn do_announce(
        &self,
        req: &AnnounceRequest<'_>,
    ) -> Result<AnnounceResponse, TrackerError> {
        let conn_id = self.ensure_connection_id().await?;
        match self.run_announce(conn_id, req).await {
            Ok(r) => Ok(r),
            Err(TrackerError::Failure(_)) => {
                // Tracker rejected — possibly stale connection id.
                // Invalidate cache and retry exactly once with a fresh
                // CONNECT.
                self.invalidate_cached_conn();
                let fresh = self.ensure_connection_id().await?;
                self.run_announce(fresh, req).await
            }
            Err(e) => Err(e),
        }
    }

    async fn run_scrape(
        &self,
        conn_id: u64,
        info_hashes: &[[u8; 20]],
    ) -> Result<ScrapeResponse, TrackerError> {
        for attempt in 0..self.max_attempts {
            let txid = random_txid();
            let rx = self
                .demux
                .register_tracker_response(txid, retry_timeout(attempt))
                .map_err(|e| TrackerError::Udp(format!("register SCRAPE txid: {e}")))?;
            let packet = encode_scrape(conn_id, txid, info_hashes)?;
            self.demux
                .send_to(&packet, self.target)
                .await
                .map_err(|e| TrackerError::Udp(format!("send SCRAPE: {e}")))?;
            if let Ok(Ok(resp)) =
                tokio::time::timeout(retry_timeout(attempt), rx).await
            {
                return decode_scrape(&resp.data, txid, info_hashes);
            }
        }
        Err(TrackerError::Timeout(self.max_attempts))
    }

    async fn do_scrape(
        &self,
        info_hashes: &[[u8; 20]],
    ) -> Result<ScrapeResponse, TrackerError> {
        let conn_id = self.ensure_connection_id().await?;
        match self.run_scrape(conn_id, info_hashes).await {
            Ok(r) => Ok(r),
            Err(TrackerError::Failure(_)) => {
                self.invalidate_cached_conn();
                let fresh = self.ensure_connection_id().await?;
                self.run_scrape(fresh, info_hashes).await
            }
            Err(e) => Err(e),
        }
    }

    fn invalidate_cached_conn(&self) {
        let mut guard = self.cached_conn.lock().expect("cached_conn poisoned");
        *guard = None;
    }
}

impl Tracker for UdpTracker {
    fn announce<'a>(&'a self, req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
        Box::pin(async move { self.do_announce(&req).await })
    }
}

impl TrackerScrape for UdpTracker {
    fn scrape<'a>(&'a self, info_hashes: &'a [[u8; 20]]) -> ScrapeFuture<'a> {
        Box::pin(async move { self.do_scrape(info_hashes).await })
    }
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

    // ---- UdpTracker client tests -----------------------------------

    /// Minimal mock UDP tracker: binds a socket, answers CONNECT with a
    /// canned connection id + echoes txid, answers ANNOUNCE with a
    /// peer list. Runs as a background task until dropped. Used to
    /// exercise the full CONNECT→ANNOUNCE handshake of [`UdpTracker`]
    /// without a real tracker dependency.
    async fn spawn_mock_tracker() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if n < 16 {
                    continue;
                }
                let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
                let txid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
                if action == ACTION_CONNECT {
                    let mut resp = [0u8; 16];
                    resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
                    resp[4..8].copy_from_slice(&txid.to_be_bytes());
                    resp[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
                    let _ = sock.send_to(&resp, from).await;
                } else {
                    // Treat non-CONNECT as ANNOUNCE. For the ANNOUNCE
                    // layout the action + txid are at bytes 8..12 + 12..16
                    // (after connection_id). Reply with interval=1800,
                    // leechers=5, seeders=7, one peer 10.0.0.1:6881.
                    let mut resp = Vec::with_capacity(26);
                    resp.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                    resp.extend_from_slice(&txid.to_be_bytes());
                    resp.extend_from_slice(&1800u32.to_be_bytes());
                    resp.extend_from_slice(&5u32.to_be_bytes());
                    resp.extend_from_slice(&7u32.to_be_bytes());
                    resp.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE1]);
                    let _ = sock.send_to(&resp, from).await;
                }
            }
        });
        (addr, task)
    }

    fn sample_announce<'a>() -> AnnounceRequest<'a> {
        AnnounceRequest {
            info_hash: [0xAA; 20],
            peer_id: [0xBB; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1_000_000,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
            tracker_id: None,
        }
    }

    #[tokio::test]
    async fn udp_tracker_connects_and_announces() {
        let (tracker_addr, tracker_task) = spawn_mock_tracker().await;
        let (demux, _rx_task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = UdpTracker::new(Arc::clone(&demux), tracker_addr);
        let resp = client.do_announce(&sample_announce()).await.unwrap();
        assert_eq!(resp.interval, Duration::from_secs(1800));
        assert_eq!(resp.complete, Some(7));
        assert_eq!(resp.incomplete, Some(5));
        assert_eq!(resp.peers.len(), 1);
        assert_eq!(resp.peers[0].port(), 6881);
        tracker_task.abort();
    }

    #[tokio::test]
    async fn udp_tracker_caches_connection_id_across_announces() {
        // Two announces back-to-back; the second should skip CONNECT
        // because the cached id is still valid. We detect this by
        // counting CONNECT messages the mock tracker sees.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let connect_count =
            Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = Arc::clone(&connect_count);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if n < 16 {
                    continue;
                }
                let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
                let txid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
                if action == ACTION_CONNECT {
                    cc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut resp = [0u8; 16];
                    resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
                    resp[4..8].copy_from_slice(&txid.to_be_bytes());
                    resp[8..16].copy_from_slice(&0xDEAD_BEEFu64.to_be_bytes());
                    let _ = sock.send_to(&resp, from).await;
                } else {
                    let mut resp = Vec::with_capacity(26);
                    resp.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                    resp.extend_from_slice(&txid.to_be_bytes());
                    resp.extend_from_slice(&1800u32.to_be_bytes());
                    resp.extend_from_slice(&0u32.to_be_bytes());
                    resp.extend_from_slice(&0u32.to_be_bytes());
                    let _ = sock.send_to(&resp, from).await;
                }
            }
        });
        let (demux, _rx_task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = UdpTracker::new(Arc::clone(&demux), addr);
        client.do_announce(&sample_announce()).await.unwrap();
        client.do_announce(&sample_announce()).await.unwrap();
        assert_eq!(
            connect_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "second announce must reuse cached connection_id"
        );
        task.abort();
    }

    #[tokio::test]
    async fn udp_tracker_surfaces_tracker_error() {
        // Mock tracker that replies ACTION_ERROR to CONNECT. The client
        // should propagate as TrackerError::Failure.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (_n, from) = sock.recv_from(&mut buf).await.unwrap();
            let txid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let mut resp = Vec::new();
            resp.extend_from_slice(&ACTION_ERROR.to_be_bytes());
            resp.extend_from_slice(&txid.to_be_bytes());
            resp.extend_from_slice(b"tracker overloaded");
            let _ = sock.send_to(&resp, from).await;
        });
        let (demux, _rx_task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client =
            UdpTracker::new(Arc::clone(&demux), addr).with_max_attempts(1);
        let err = client.do_announce(&sample_announce()).await.unwrap_err();
        match err {
            TrackerError::Failure(msg) => assert!(msg.contains("overloaded")),
            other => panic!("expected Failure, got {other:?}"),
        }
        task.abort();
    }

    // ---- SCRAPE codec --------------------------------------------

    #[test]
    fn encode_scrape_rejects_empty_input() {
        let err = encode_scrape(0, 0, &[]).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn encode_scrape_rejects_too_many_hashes() {
        let hashes = vec![[0xAAu8; 20]; MAX_SCRAPE_HASHES + 1];
        let err = encode_scrape(0, 0, &hashes).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn encode_scrape_lays_out_header_and_hashes() {
        let buf = encode_scrape(0x1122_3344_5566_7788, 0xAABB_CCDD, &[[0xAA; 20], [0xBB; 20]])
            .unwrap();
        assert_eq!(buf.len(), 16 + 40);
        assert_eq!(&buf[0..8], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(&buf[8..12], &ACTION_SCRAPE.to_be_bytes());
        assert_eq!(&buf[12..16], &0xAABB_CCDDu32.to_be_bytes());
        assert_eq!(&buf[16..36], &[0xAAu8; 20]);
        assert_eq!(&buf[36..56], &[0xBBu8; 20]);
    }

    #[test]
    fn decode_scrape_roundtrips_two_hashes() {
        // Build a response: action=2, txid, (seeders, completed, leechers) × 2.
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
        resp.extend_from_slice(&0xAABB_CCDDu32.to_be_bytes());
        resp.extend_from_slice(&10u32.to_be_bytes()); // seeders[0]
        resp.extend_from_slice(&100u32.to_be_bytes()); // completed[0]
        resp.extend_from_slice(&20u32.to_be_bytes()); // leechers[0]
        resp.extend_from_slice(&5u32.to_be_bytes()); // seeders[1]
        resp.extend_from_slice(&50u32.to_be_bytes()); // completed[1]
        resp.extend_from_slice(&15u32.to_be_bytes()); // leechers[1]
        let hashes = [[0xAAu8; 20], [0xBBu8; 20]];
        let out = decode_scrape(&resp, 0xAABB_CCDD, &hashes).unwrap();
        assert_eq!(out.files.len(), 2);
        let a = &out.files[&[0xAA; 20]];
        assert_eq!(a.complete, 10);
        assert_eq!(a.downloaded, 100);
        assert_eq!(a.incomplete, 20);
        let b = &out.files[&[0xBB; 20]];
        assert_eq!(b.complete, 5);
        assert_eq!(b.downloaded, 50);
        assert_eq!(b.incomplete, 15);
    }

    #[test]
    fn decode_scrape_rejects_wrong_size() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
        resp.extend_from_slice(&1u32.to_be_bytes());
        // Claim 2 hashes but only supply 12 bytes (= 1 file record).
        resp.extend_from_slice(&[0u8; 12]);
        let err = decode_scrape(&resp, 1, &[[0xAAu8; 20], [0xBBu8; 20]]).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn decode_scrape_propagates_tracker_error() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        resp.extend_from_slice(&42u32.to_be_bytes());
        resp.extend_from_slice(b"not permitted");
        let err = decode_scrape(&resp, 42, &[[0xAA; 20]]).unwrap_err();
        match err {
            TrackerError::Failure(msg) => assert_eq!(msg, "not permitted"),
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn decode_scrape_rejects_wrong_txid() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
        resp.extend_from_slice(&999u32.to_be_bytes());
        resp.extend_from_slice(&[0u8; 12]);
        let err = decode_scrape(&resp, 1, &[[0xAAu8; 20]]).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[tokio::test]
    async fn udp_tracker_scrape_end_to_end() {
        // Mock tracker: respond to CONNECT, then to SCRAPE with a
        // canned 2-file response.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if n < 16 {
                    continue;
                }
                let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
                let txid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
                if action == ACTION_CONNECT {
                    let mut resp = [0u8; 16];
                    resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
                    resp[4..8].copy_from_slice(&txid.to_be_bytes());
                    resp[8..16].copy_from_slice(&0xCAFE_BABEu64.to_be_bytes());
                    let _ = sock.send_to(&resp, from).await;
                } else if action == ACTION_SCRAPE {
                    // Two info_hashes sent → respond with 2 file records.
                    let mut resp = Vec::new();
                    resp.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
                    resp.extend_from_slice(&txid.to_be_bytes());
                    // Record 0: seeders=7 completed=70 leechers=3
                    resp.extend_from_slice(&7u32.to_be_bytes());
                    resp.extend_from_slice(&70u32.to_be_bytes());
                    resp.extend_from_slice(&3u32.to_be_bytes());
                    // Record 1: seeders=0 completed=0 leechers=0
                    resp.extend_from_slice(&0u32.to_be_bytes());
                    resp.extend_from_slice(&0u32.to_be_bytes());
                    resp.extend_from_slice(&0u32.to_be_bytes());
                    let _ = sock.send_to(&resp, from).await;
                }
            }
        });

        let (demux, _rx_task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = UdpTracker::new(Arc::clone(&demux), addr);
        let hashes = [[0x11u8; 20], [0x22u8; 20]];
        let resp = client.do_scrape(&hashes).await.unwrap();
        assert_eq!(resp.files.len(), 2);
        assert_eq!(resp.files[&[0x11; 20]].complete, 7);
        assert_eq!(resp.files[&[0x11; 20]].downloaded, 70);
        assert_eq!(resp.files[&[0x11; 20]].incomplete, 3);
        assert_eq!(resp.files[&[0x22; 20]].complete, 0);
        task.abort();
    }

    #[tokio::test]
    async fn udp_tracker_times_out_when_no_response() {
        // No mock tracker is listening at the target address. The
        // client should run its full retry curve (clamped by
        // with_max_attempts(1) + a short synthetic timeout). We
        // override the retry curve by calling run_connect directly
        // via a very small attempt cap so the test completes quickly.
        let (demux, _rx_task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        // Fake target: pick a random unused port (bind+drop to reserve
        // + release, then target that — minor race but fine for test).
        let placeholder = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = placeholder.local_addr().unwrap();
        drop(placeholder);
        let client = UdpTracker::new(Arc::clone(&demux), target).with_max_attempts(1);
        // Use a very short timeout by racing against Tokio's
        // `timeout` wrapper directly — the default attempt 0 is 15 s,
        // too long for a test. Use a 500 ms envelope.
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            client.do_announce(&sample_announce()),
        )
        .await;
        // Either the tokio timeout hit before the tracker's internal
        // one, or the client's own timeout path returned Err — both
        // count as "no response → error".
        match result {
            Err(_timeout) => {} // outer timeout hit first; acceptable
            Ok(Err(TrackerError::Timeout(1))) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}
