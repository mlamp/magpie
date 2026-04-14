//! UDP demultiplexer.
//!
//! Single [`tokio::net::UdpSocket`]; a background task owns the recv loop and
//! fans out packets to the right subscriber. For M2 only one subscriber class
//! exists (tracker responses, routed by the 4-byte transaction id from BEP
//! 15); M3 DHT (`b'd'` bencode prefix) and M4 uTP (first byte in
//! `0x01|0x11|0x21|0x31|0x41`) register in later milestones without disturbing
//! the M2 shape.
//!
//! Tracker flow:
//!
//! ```text
//! tracker: register_tracker_response(txid, ttl)  →  oneshot::Receiver
//! tracker: send_to(req_bytes, addr)
//!                                   ← recv loop reads response, bytes [4..8]
//!                                     give transaction_id → delivers to
//!                                     the oneshot; entry expires via TTL if
//!                                     the response never arrives.
//! ```
//!
//! TTL defence: a malicious / broken tracker that never responds must not
//! leak a `oneshot::Sender` forever. The recv-loop clock sweeps the pending
//! map on every iteration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Maximum size of a UDP datagram we'll allocate a recv buffer for.
/// BEP 15 announce responses are 20-byte header + 6 bytes per peer. Sized at
/// 8 KiB so that trackers returning up to ~1300 peers in a single datagram
/// are not truncated. Well under typical UDP read burst size; the buffer is
/// one-shot per `recv_from` call.
const RECV_BUFFER_SIZE: usize = 8192;

/// Default TTL for tracker transaction registrations (BEP 15 scrape/announce
/// retries happen within this window).
pub const DEFAULT_TRACKER_TXN_TTL: Duration = Duration::from_secs(60);

/// Cap on the number of simultaneously pending tracker transactions per
/// demux. Per ADR-0015; guards against a runaway tracker from pinning
/// unbounded memory.
const MAX_PENDING_TRACKER_TXNS: usize = 10_000;

/// A tracker response delivered to the channel returned by
/// [`UdpDemux::register_tracker_response`].
#[derive(Debug)]
pub struct TrackerResponse {
    /// Source address — may differ from the target for NAT'd trackers.
    pub from: SocketAddr,
    /// Response bytes, including the 4-byte action + 4-byte `transaction_id`
    /// header.
    pub data: Vec<u8>,
}

/// Errors surfaced by the demux.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DemuxError {
    /// Underlying socket I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Too many pending tracker transactions.
    #[error("tracker txn cap reached ({MAX_PENDING_TRACKER_TXNS})")]
    TooManyPendingTransactions,
    /// A transaction id is already registered.
    #[error("transaction id {0} already registered")]
    DuplicateTransactionId(u32),
}

struct PendingTxn {
    sender: oneshot::Sender<TrackerResponse>,
    expires_at: Instant,
}

/// UDP demux with a single bound socket and shared pending-transactions map.
///
/// Typical usage: call [`UdpDemux::bind`], keep the returned [`Arc<Self>`],
/// and clone it into tracker / DHT / uTP subsystems as needed. The recv-loop
/// task ([`JoinHandle`]) runs until every [`Arc<UdpDemux>`] clone is dropped
/// **and** the socket is closed — in practice, hold a clone for the lifetime
/// of the [`crate::engine::Engine`].
pub struct UdpDemux {
    socket: Arc<UdpSocket>,
    pending: Arc<StdMutex<HashMap<u32, PendingTxn>>>,
    /// Dropped-packet counter for packets that matched no subscriber — useful
    /// for alerting on misconfigured trackers / crossed wires. Currently
    /// exposed via [`UdpDemux::dropped_unmatched`].
    dropped_unmatched: Arc<std::sync::atomic::AtomicU64>,
}

impl UdpDemux {
    /// Bind a socket and start the recv loop. Returns the demux handle and
    /// the background task's [`JoinHandle`].
    ///
    /// # Errors
    ///
    /// Surfaces the first bind error from `UdpSocket::bind`.
    pub async fn bind(addr: SocketAddr) -> Result<(Arc<Self>, JoinHandle<()>), std::io::Error> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        let demux = Arc::new(Self {
            socket: Arc::clone(&socket),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            dropped_unmatched: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        let recv_demux = Arc::clone(&demux);
        let task = tokio::spawn(async move { recv_demux.run_recv_loop().await });
        Ok((demux, task))
    }

    /// Bound local address.
    ///
    /// # Errors
    ///
    /// Surfaces any error from `UdpSocket::local_addr`.
    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.socket.local_addr()
    }

    /// Register interest in the tracker response for `transaction_id`.
    /// Caller should send its request packet *after* registering. Returns a
    /// one-shot receiver that fires with the matching response, or is
    /// dropped on TTL expiry.
    ///
    /// # Errors
    ///
    /// Returns [`DemuxError::TooManyPendingTransactions`] past the global
    /// cap, or [`DemuxError::DuplicateTransactionId`] for re-registration
    /// collisions.
    ///
    /// # Panics
    ///
    /// Panics only if the internal mutex is poisoned, which requires a prior
    /// panic in a critical section — structurally unreachable in production.
    pub fn register_tracker_response(
        &self,
        transaction_id: u32,
        ttl: Duration,
    ) -> Result<oneshot::Receiver<TrackerResponse>, DemuxError> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().expect("tracker pending poisoned");
        if guard.len() >= MAX_PENDING_TRACKER_TXNS {
            return Err(DemuxError::TooManyPendingTransactions);
        }
        if guard.contains_key(&transaction_id) {
            return Err(DemuxError::DuplicateTransactionId(transaction_id));
        }
        guard.insert(
            transaction_id,
            PendingTxn { sender: tx, expires_at: Instant::now() + ttl },
        );
        drop(guard);
        Ok(rx)
    }

    /// Send a datagram. Tracker clients use this after registering for the
    /// response.
    ///
    /// # Errors
    ///
    /// Surfaces the underlying socket error.
    pub async fn send_to(&self, data: &[u8], target: SocketAddr) -> Result<usize, std::io::Error> {
        self.socket.send_to(data, target).await
    }

    /// Observability hook — count of unmatched datagrams since bind.
    #[must_use]
    pub fn dropped_unmatched(&self) -> u64 {
        self.dropped_unmatched.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn run_recv_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; RECV_BUFFER_SIZE];
        loop {
            self.sweep_expired();
            let (len, from) = match self.socket.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(e) => {
                    // Socket closed or transient OS error. Logging only; the
                    // demux keeps trying on non-fatal errors. `Other` is a
                    // fatal sentinel used by some platforms for "socket is
                    // gone"; everything else gets a 10 ms backoff to avoid
                    // hot-spinning on persistent ICMP-unreachable storms.
                    tracing::debug!(error = %e, "udp demux recv_from failed");
                    if e.kind() == std::io::ErrorKind::Other {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            self.dispatch(&buf[..len], from);
        }
    }

    fn dispatch(&self, packet: &[u8], from: SocketAddr) {
        // BEP 15 response header: [action: u32 BE][transaction_id: u32 BE][body…].
        // We route by transaction_id; first-byte classification (DHT/uTP) is
        // a future hook not wired in M2.
        if packet.len() < 8 {
            self.dropped_unmatched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let txid = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let sender = {
            let mut guard = self.pending.lock().expect("tracker pending poisoned");
            guard.remove(&txid).map(|p| p.sender)
        };
        if let Some(sender) = sender {
            let _ = sender.send(TrackerResponse { from, data: packet.to_vec() });
        } else {
            self.dropped_unmatched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn sweep_expired(&self) {
        let now = Instant::now();
        let mut guard = self.pending.lock().expect("tracker pending poisoned");
        guard.retain(|_, entry| entry.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracker_txid_roundtrip() {
        // Two demuxen on loopback; one acts as a tracker, the other as a
        // client. The client registers a txid, sends a request, the
        // "tracker" echoes a response with the same txid, and the client
        // receives it via the registered oneshot.
        let (client, _task_c) = UdpDemux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let (tracker, _task_t) = UdpDemux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let tracker_addr = tracker.local_addr().unwrap();

        let txid: u32 = 0xCAFE_BABE;
        let rx = client.register_tracker_response(txid, Duration::from_secs(5)).unwrap();

        // Client sends a 16-byte request; tracker fabricates a response with
        // the same txid in bytes 4..8.
        let mut req = [0u8; 16];
        req[4..8].copy_from_slice(&txid.to_be_bytes());
        let client_addr = client.local_addr().unwrap();
        client.send_to(&req, tracker_addr).await.unwrap();

        // The tracker-side demux won't route to any subscriber (none
        // registered), so we bypass it and respond via its raw socket. Use a
        // fresh UdpSocket for the tracker's reply.
        let tracker_reply = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut resp = [0u8; 20];
        resp[..4].copy_from_slice(&1u32.to_be_bytes()); // action = announce
        resp[4..8].copy_from_slice(&txid.to_be_bytes());
        tracker_reply.send_to(&resp, client_addr).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), rx).await.unwrap().unwrap();
        assert_eq!(result.data.len(), 20);
        assert_eq!(&result.data[4..8], &txid.to_be_bytes());
    }

    #[tokio::test]
    async fn duplicate_txid_rejected() {
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let _rx = demux.register_tracker_response(42, Duration::from_secs(5)).unwrap();
        let err = demux.register_tracker_response(42, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, DemuxError::DuplicateTransactionId(42)));
    }

    #[tokio::test]
    async fn short_packet_counted_as_unmatched() {
        let (client, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send a 4-byte packet (shorter than the 8-byte minimum header).
        peer.send_to(&[0u8; 4], client.local_addr().unwrap()).await.unwrap();
        // Poll until the counter increments or we time out.
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.dropped_unmatched() == 0 {
            assert!(Instant::now() <= deadline, "dropped_unmatched never incremented");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(client.dropped_unmatched() >= 1);
    }
}
