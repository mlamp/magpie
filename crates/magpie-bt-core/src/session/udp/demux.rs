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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
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

/// A UDP datagram delivered to a first-byte-dispatched subscriber
/// ([`UdpDemux::register_dht`]). Per ADR-0015 § "Shape".
#[derive(Debug, Clone)]
pub struct UdpPacket {
    /// Raw datagram payload.
    pub data: Vec<u8>,
    /// Sender address.
    pub from: SocketAddr,
    /// Instant the recv loop handed the packet off — used by
    /// downstream subsystems that care about per-packet age.
    pub received_at: Instant,
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
    /// A DHT subscriber is already registered on this demux.
    #[error("DHT subscriber already registered")]
    DhtAlreadyRegistered,
}

#[derive(Debug)]
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
#[derive(Debug)]
pub struct UdpDemux {
    socket: Arc<UdpSocket>,
    pending: Arc<StdMutex<HashMap<u32, PendingTxn>>>,
    /// Dropped-packet counter for packets that matched no subscriber — useful
    /// for alerting on misconfigured trackers / crossed wires. Currently
    /// exposed via [`UdpDemux::dropped_unmatched`].
    dropped_unmatched: Arc<std::sync::atomic::AtomicU64>,
    /// First-byte-`b'd'` subscriber — set once via
    /// [`UdpDemux::register_dht`]. Lock-free on the hot path.
    dht_tx: OnceLock<mpsc::Sender<UdpPacket>>,
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
            dht_tx: OnceLock::new(),
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
            PendingTxn {
                sender: tx,
                expires_at: Instant::now() + ttl,
            },
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

    /// Register the DHT subscriber. Every inbound datagram whose first
    /// byte is `b'd'` (the bencode dict opener — every KRPC message
    /// starts with one) is forwarded to `tx`.
    ///
    /// Per ADR-0015 the tracker action byte (`0x00`) and uTP discriminators
    /// (`0x01`, `0x11`, `0x21`, `0x31`, `0x41`) do not collide with `b'd'`
    /// (`0x64`), so DHT dispatch never steals tracker traffic.
    ///
    /// # Errors
    ///
    /// [`DemuxError::DhtAlreadyRegistered`] when called more than once —
    /// the subscriber is a single-slot [`std::sync::OnceLock`].
    pub fn register_dht(&self, tx: mpsc::Sender<UdpPacket>) -> Result<(), DemuxError> {
        self.dht_tx
            .set(tx)
            .map_err(|_| DemuxError::DhtAlreadyRegistered)
    }

    /// Observability hook — count of unmatched datagrams since bind.
    #[must_use]
    pub fn dropped_unmatched(&self) -> u64 {
        self.dropped_unmatched
            .load(std::sync::atomic::Ordering::Relaxed)
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
        // First-byte classification (ADR-0015). DHT packets always start
        // with `b'd'` (bencode dict); tracker responses start with a
        // BEP 15 `action` u32 BE that is always `0x00` in the first
        // byte (values 0–3). uTP is wired in a later milestone.
        if packet.is_empty() {
            self.dropped_unmatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if packet[0] == b'd' {
            self.dispatch_dht(packet, from);
            return;
        }
        self.dispatch_tracker(packet, from);
    }

    fn dispatch_dht(&self, packet: &[u8], from: SocketAddr) {
        let Some(tx) = self.dht_tx.get() else {
            self.dropped_unmatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        // `try_send` so a wedged DHT task cannot block the recv loop
        // (and therefore the tracker path) — per ADR-0015 backpressure
        // policy.
        if tx
            .try_send(UdpPacket {
                data: packet.to_vec(),
                from,
                received_at: Instant::now(),
            })
            .is_err()
        {
            self.dropped_unmatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn dispatch_tracker(&self, packet: &[u8], from: SocketAddr) {
        // BEP 15 response header: [action: u32 BE][transaction_id: u32 BE][body…].
        if packet.len() < 8 {
            self.dropped_unmatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let txid = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let sender = {
            let mut guard = self.pending.lock().expect("tracker pending poisoned");
            guard.remove(&txid).map(|p| p.sender)
        };
        if let Some(sender) = sender {
            let _ = sender.send(TrackerResponse {
                from,
                data: packet.to_vec(),
            });
        } else {
            self.dropped_unmatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let (client, _task_c) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (tracker, _task_t) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let tracker_addr = tracker.local_addr().unwrap();

        let txid: u32 = 0xCAFE_BABE;
        let rx = client
            .register_tracker_response(txid, Duration::from_secs(5))
            .unwrap();

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

        let result = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.data.len(), 20);
        assert_eq!(&result.data[4..8], &txid.to_be_bytes());
    }

    #[tokio::test]
    async fn dht_dispatch_routes_bencode_dict_to_subscriber() {
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel::<UdpPacket>(8);
        demux.register_dht(tx).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender_addr = sender.local_addr().unwrap();

        // Minimal valid bencode dict (`de`) — empty dict.
        sender
            .send_to(b"de", demux.local_addr().unwrap())
            .await
            .unwrap();

        let packet = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .expect("dht subscriber delivered packet");
        assert_eq!(packet.data, b"de");
        assert_eq!(packet.from, sender_addr);
        assert_eq!(demux.dropped_unmatched(), 0);
    }

    #[tokio::test]
    async fn dht_dispatch_drops_when_no_subscriber_registered() {
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(b"de", demux.local_addr().unwrap())
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while demux.dropped_unmatched() == 0 {
            assert!(
                Instant::now() <= deadline,
                "dropped_unmatched never incremented"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn register_dht_twice_rejected() {
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (tx1, _rx1) = mpsc::channel::<UdpPacket>(1);
        demux.register_dht(tx1).unwrap();
        let (tx2, _rx2) = mpsc::channel::<UdpPacket>(1);
        let err = demux.register_dht(tx2).unwrap_err();
        assert!(matches!(err, DemuxError::DhtAlreadyRegistered));
    }

    #[tokio::test]
    async fn dht_does_not_steal_tracker_responses() {
        // A response whose first byte is 0x00 (tracker action) must
        // still route via the tracker txid path, not the DHT branch.
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        // Register DHT so the branch is live; we still expect tracker
        // traffic to bypass it.
        let (dht_tx, mut dht_rx) = mpsc::channel::<UdpPacket>(8);
        demux.register_dht(dht_tx).unwrap();

        let txid: u32 = 0xDEAD_BEEF;
        let tracker_rx = demux
            .register_tracker_response(txid, Duration::from_secs(5))
            .unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut resp = [0u8; 20];
        resp[..4].copy_from_slice(&1u32.to_be_bytes());
        resp[4..8].copy_from_slice(&txid.to_be_bytes());
        sender
            .send_to(&resp, demux.local_addr().unwrap())
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(2), tracker_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got.data[4..8], &txid.to_be_bytes());
        // Nothing on the DHT channel.
        assert!(dht_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicate_txid_rejected() {
        let (demux, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let _rx = demux
            .register_tracker_response(42, Duration::from_secs(5))
            .unwrap();
        let err = demux
            .register_tracker_response(42, Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, DemuxError::DuplicateTransactionId(42)));
    }

    #[tokio::test]
    async fn short_packet_counted_as_unmatched() {
        let (client, _task) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send a 4-byte packet (shorter than the 8-byte minimum header).
        peer.send_to(&[0u8; 4], client.local_addr().unwrap())
            .await
            .unwrap();
        // Poll until the counter increments or we time out.
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.dropped_unmatched() == 0 {
            assert!(
                Instant::now() <= deadline,
                "dropped_unmatched never incremented"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(client.dropped_unmatched() >= 1);
    }
}
