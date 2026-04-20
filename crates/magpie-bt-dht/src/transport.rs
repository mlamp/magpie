//! DHT transport plumbing — channel-driven inbound / outbound pumps
//! with 2-byte-txid query tracking.
//!
//! The real UDP socket lives behind [`Datagram`]-typed channels: an
//! inbound `mpsc::Receiver<Datagram>` fed by the owner's datagram
//! source (typically `magpie-bt-core`'s `UdpDemux`) and an outbound
//! `mpsc::Sender<Datagram>` drained by the owner's send loop. This
//! keeps `magpie-bt-dht` free of any dependency on `magpie-bt-core`;
//! the wiring adapter lives on the core side behind the `dht`
//! feature flag.
//!
//! # Shape
//!
//! ```text
//!                     ┌────────────────────────┐
//!     UdpDemux ──────►│ inbound: mpsc<Datagram>│
//!                     │                        │
//!                     │         Dht            │──► inbound_queries:
//!                     │                        │    mpsc<InboundQuery>
//!                     │                        │
//!                     │outbound: mpsc<Datagram>│──► send loop ──► UDP
//!                     └────────────────────────┘
//! ```
//!
//! The `Dht` task:
//!
//! 1. Pumps `inbound`: decode each datagram with the KRPC codec;
//!    responses / errors route to pending-query oneshots by txid;
//!    queries land in the `inbound_queries` channel for handler
//!    consumption.
//! 2. Services [`Dht::send_query`]: allocates a 2-byte txid,
//!    registers a oneshot, pushes the encoded query onto `outbound`,
//!    and returns the receiver (or a timeout / remote error).
//!
//! Workstream-B scope: this module + tests with in-memory channel
//! pairs. Wiring to `UdpDemux` is workstream B-tail / G.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use thiserror::Error;

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::krpc::{KrpcErrorPayload, KrpcKind, KrpcMessage, Query, Response};

// ---------------------------------------------------------------------------
// Config + data types
// ---------------------------------------------------------------------------

/// Default timeout for outstanding [`Dht::send_query`] calls.
///
/// Kademlia latencies are well under a second for reachable nodes;
/// 5 s covers the long tail (NAT-constrained paths, packet loss).
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Default outbound channel capacity. Large enough that a burst of
/// `find_node` from a lookup doesn't backpressure callers; small
/// enough to surface bugs where the send loop has stalled.
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 1024;

/// Default inbound-queries channel capacity.
pub const DEFAULT_INBOUND_QUERIES_CAPACITY: usize = 1024;

/// Cap on simultaneously outstanding outbound queries.
///
/// The full `u16` txid space allows 65 536 distinct ids; we cap
/// well below that (matching ADR-0015's tracker-side 10 000 cap) so
/// a single DHT task cannot pin unbounded oneshot state even under
/// pathological churn.
pub const MAX_PENDING_QUERIES: usize = 10_000;

/// A UDP-layer datagram, used for both inbound and outbound traffic
/// on the [`Dht`] pumps.
#[derive(Debug, Clone)]
pub struct Datagram {
    /// Raw KRPC-bencode bytes.
    pub data: Vec<u8>,
    /// Peer address. `from` for inbound, `target` for outbound.
    pub addr: SocketAddr,
}

/// Runtime knobs for [`Dht::spawn`].
#[derive(Debug, Clone)]
pub struct DhtConfig {
    /// Deadline past which an outstanding `send_query` resolves to
    /// [`QueryError::Timeout`]. Default [`DEFAULT_QUERY_TIMEOUT`].
    pub query_timeout: Duration,
    /// Capacity of the `inbound_queries` mpsc ([`Dht`] is the sender).
    pub inbound_queries_capacity: usize,
    /// Capacity of the outbound mpsc ([`Dht`] is the sender).
    pub outbound_capacity: usize,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            query_timeout: DEFAULT_QUERY_TIMEOUT,
            inbound_queries_capacity: DEFAULT_INBOUND_QUERIES_CAPACITY,
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
        }
    }
}

/// An inbound query delivered to the user for handling.
///
/// The user's handler should build a [`Response`] or
/// [`KrpcErrorPayload`], wrap it into a [`KrpcMessage`] with the
/// same `transaction_id`, encode, and push onto the outbound channel
/// via [`Dht::respond`].
#[derive(Debug, Clone)]
pub struct InboundQuery {
    /// Remote transaction id — echo verbatim in the response.
    pub transaction_id: Vec<u8>,
    /// The decoded query body.
    pub query: Query,
    /// Sender's UDP address (where the response must go).
    pub from: SocketAddr,
    /// Optional `v` string the remote advertised.
    pub client_version: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures for [`Dht::send_query`].
#[derive(Debug, Error)]
pub enum QueryError {
    /// The query didn't get a matching response within the config'd
    /// timeout.
    #[error("DHT query timed out")]
    Timeout,
    /// The peer replied with a KRPC error (y = "e").
    #[error("DHT query rejected: code {code} {message:?}")]
    Remote {
        /// BEP 5 error code (201 generic / 202 server / 203 protocol / 204 method-unknown).
        code: i32,
        /// Description text.
        message: String,
    },
    /// Outbound channel is closed — the send loop has shut down.
    #[error("outbound channel closed")]
    OutboundClosed,
    /// Pending-query map is saturated (see [`MAX_PENDING_QUERIES`]).
    #[error("pending-query table saturated")]
    TooManyPending,
    /// The Dht task has shut down and cleared pending queries before
    /// a response arrived.
    #[error("Dht task shut down")]
    Shutdown,
}

// ---------------------------------------------------------------------------
// Dht
// ---------------------------------------------------------------------------

/// A running DHT task handle — clone cheaply, store alongside the
/// engine.
#[derive(Debug, Clone)]
pub struct Dht {
    inner: Arc<DhtInner>,
}

#[derive(Debug)]
struct DhtInner {
    config: DhtConfig,
    outbound: mpsc::Sender<Datagram>,
    pending: Mutex<HashMap<u16, oneshot::Sender<QueryOutcome>>>,
    next_txid: AtomicU16,
}

#[derive(Debug)]
enum QueryOutcome {
    Response(Response),
    Error(KrpcErrorPayload),
}

impl Dht {
    /// Spawn the DHT inbound pump with caller-provided channel halves.
    ///
    /// Wiring:
    /// * `inbound` is fed by the transport layer's datagram-receive
    ///   side (e.g. `UdpDemux`'s first-byte-`b'd'` dispatch).
    /// * `outbound` is drained by the transport layer's send loop.
    /// * `inbound_queries` surfaces decoded inbound queries to the
    ///   handler consumer — drain it and call [`Dht::respond`] to
    ///   reply.
    ///
    /// Returns the caller-facing [`Dht`] handle (clone freely) and
    /// the pump `JoinHandle`. The pump owns `inbound` and exits
    /// when that receiver closes.
    #[must_use]
    pub fn spawn(
        config: DhtConfig,
        inbound: mpsc::Receiver<Datagram>,
        outbound: mpsc::Sender<Datagram>,
        inbound_queries: mpsc::Sender<InboundQuery>,
    ) -> (Self, JoinHandle<()>) {
        // Randomise the txid counter start so predictable sequencing
        // doesn't help any on-path spoofer. Failure is tolerable:
        // txids aren't required to be secret, just per-outstanding-
        // query unique.
        let mut seed = [0u8; 2];
        let _ = getrandom::fill(&mut seed);
        let start = u16::from_le_bytes(seed);

        let inner = Arc::new(DhtInner {
            config,
            outbound,
            pending: Mutex::new(HashMap::new()),
            next_txid: AtomicU16::new(start),
        });

        let pump_inner = Arc::clone(&inner);
        let join = tokio::spawn(run_inbound_pump(pump_inner, inbound, inbound_queries));

        (Self { inner }, join)
    }

    /// Send a query to `target`, awaiting its matching response.
    ///
    /// # Errors
    ///
    /// [`QueryError`] variants:
    /// - [`QueryError::Timeout`] — no reply within
    ///   [`DhtConfig::query_timeout`].
    /// - [`QueryError::Remote`] — the remote replied with a KRPC
    ///   error.
    /// - [`QueryError::OutboundClosed`] — the outbound channel has
    ///   been closed (caller's send loop gone).
    /// - [`QueryError::TooManyPending`] — pending-query table
    ///   saturated at [`MAX_PENDING_QUERIES`].
    /// - [`QueryError::Shutdown`] — the inbound pump has dropped
    ///   the pending map before a response arrived.
    pub async fn send_query(
        &self,
        target: SocketAddr,
        query: Query,
    ) -> Result<Response, QueryError> {
        // Allocate a txid + oneshot slot under one lock acquisition.
        let mut pending = self.inner.pending.lock().await;
        if pending.len() >= MAX_PENDING_QUERIES {
            return Err(QueryError::TooManyPending);
        }
        let txid = self.alloc_txid(&pending);
        let (tx, rx) = oneshot::channel();
        pending.insert(txid, tx);
        drop(pending);

        let msg = KrpcMessage {
            transaction_id: txid.to_be_bytes().to_vec(),
            kind: KrpcKind::Query(query),
            client_version: None,
            ip: None,
        };
        let datagram = Datagram {
            data: msg.encode(),
            addr: target,
        };

        // Send. If the channel is closed, purge our pending entry so
        // we don't leak a oneshot slot.
        if self.inner.outbound.send(datagram).await.is_err() {
            self.inner.pending.lock().await.remove(&txid);
            return Err(QueryError::OutboundClosed);
        }

        // Await the response with the config's timeout.
        let outcome = match timeout(self.inner.config.query_timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                // oneshot sender dropped without sending — the pump
                // shut down between registration and response.
                return Err(QueryError::Shutdown);
            }
            Err(_) => {
                // Timeout — purge our pending entry.
                self.inner.pending.lock().await.remove(&txid);
                return Err(QueryError::Timeout);
            }
        };

        match outcome {
            QueryOutcome::Response(r) => Ok(r),
            QueryOutcome::Error(e) => Err(QueryError::Remote {
                code: e.code,
                message: e.message,
            }),
        }
    }

    /// Push a pre-built response/error back to `target`. Used by the
    /// inbound-queries consumer after servicing an `InboundQuery`.
    ///
    /// # Errors
    ///
    /// [`QueryError::OutboundClosed`] if the outbound channel is
    /// closed.
    pub async fn respond(
        &self,
        transaction_id: Vec<u8>,
        kind: ResponseKind,
        target: SocketAddr,
    ) -> Result<(), QueryError> {
        let msg = KrpcMessage {
            transaction_id,
            kind: match kind {
                ResponseKind::Response(r) => KrpcKind::Response(r),
                ResponseKind::Error(e) => KrpcKind::Error(e),
            },
            client_version: None,
            ip: None,
        };
        self.inner
            .outbound
            .send(Datagram {
                data: msg.encode(),
                addr: target,
            })
            .await
            .map_err(|_| QueryError::OutboundClosed)
    }

    /// Current count of outstanding outbound queries (for metrics).
    pub async fn pending_query_count(&self) -> usize {
        self.inner.pending.lock().await.len()
    }

    /// Allocate an unused txid. Caller holds the `pending` lock.
    /// Probing is bounded at the full `u16` space; returns the
    /// counter-advanced id even on saturation (caller already
    /// validated capacity).
    fn alloc_txid(&self, pending: &HashMap<u16, oneshot::Sender<QueryOutcome>>) -> u16 {
        for _ in 0..=u32::from(u16::MAX) {
            let txid = self.inner.next_txid.fetch_add(1, Ordering::Relaxed);
            if !pending.contains_key(&txid) {
                return txid;
            }
        }
        // Full ring probed without finding a free slot — the caller
        // already validated `pending.len() < MAX_PENDING_QUERIES`, so
        // this is structurally unreachable (max pending < 65 536 ids).
        self.inner.next_txid.fetch_add(1, Ordering::Relaxed)
    }
}

/// Discriminator for [`Dht::respond`] — a handler produces either
/// a successful [`Response`] or a [`KrpcErrorPayload`].
#[derive(Debug, Clone)]
pub enum ResponseKind {
    /// `y = "r"` — success.
    Response(Response),
    /// `y = "e"` — rejection.
    Error(KrpcErrorPayload),
}

// ---------------------------------------------------------------------------
// Pump
// ---------------------------------------------------------------------------

async fn run_inbound_pump(
    inner: Arc<DhtInner>,
    mut inbound: mpsc::Receiver<Datagram>,
    inbound_queries: mpsc::Sender<InboundQuery>,
) {
    while let Some(datagram) = inbound.recv().await {
        process_inbound(&inner, &inbound_queries, datagram).await;
    }

    // Channel closed — purge any pending oneshot senders so their
    // `send_query` callers wake up with `QueryError::Shutdown`
    // rather than waiting forever.
    let mut pending = inner.pending.lock().await;
    pending.clear();
}

async fn process_inbound(
    inner: &Arc<DhtInner>,
    inbound_queries: &mpsc::Sender<InboundQuery>,
    datagram: Datagram,
) {
    let msg = match KrpcMessage::decode(&datagram.data) {
        Ok(m) => m,
        Err(err) => {
            tracing::debug!(
                from = %datagram.addr,
                error = %err,
                "dropping malformed KRPC datagram",
            );
            return;
        }
    };

    // Our own outbound queries always use 2-byte txids; replies
    // with a different txid length cannot match a pending entry
    // (covers unsolicited replies and obvious spoofing).
    match msg.kind {
        KrpcKind::Query(q) => {
            // Don't block the pump on a saturated query consumer.
            if let Err(e) = inbound_queries.try_send(InboundQuery {
                transaction_id: msg.transaction_id,
                query: q,
                from: datagram.addr,
                client_version: msg.client_version,
            }) {
                tracing::debug!(
                    from = %datagram.addr,
                    error = %e,
                    "inbound_queries channel full/closed; dropping query",
                );
            }
        }
        KrpcKind::Response(r) => {
            if msg.transaction_id.len() == 2 {
                route_reply(inner, &msg.transaction_id, QueryOutcome::Response(r)).await;
            } else {
                tracing::debug!(
                    from = %datagram.addr,
                    txid_len = msg.transaction_id.len(),
                    "ignoring response with non-2-byte txid",
                );
            }
        }
        KrpcKind::Error(e) => {
            if msg.transaction_id.len() == 2 {
                route_reply(inner, &msg.transaction_id, QueryOutcome::Error(e)).await;
            } else {
                tracing::debug!(
                    from = %datagram.addr,
                    txid_len = msg.transaction_id.len(),
                    "ignoring error with non-2-byte txid",
                );
            }
        }
    }
}

async fn route_reply(inner: &Arc<DhtInner>, txid: &[u8], outcome: QueryOutcome) {
    debug_assert_eq!(txid.len(), 2, "caller validated txid length");
    let txid = u16::from_be_bytes([txid[0], txid[1]]);
    let sender = {
        let mut pending = inner.pending.lock().await;
        pending.remove(&txid)
    };
    if let Some(sender) = sender {
        let _ = sender.send(outcome);
    } else {
        tracing::debug!(txid, "reply for unknown/expired txid");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::krpc::{KrpcErrorPayload, KrpcKind, KrpcMessage, Query, Response};
    use crate::node_id::NodeId;

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn ping_query() -> Query {
        Query::Ping {
            id: NodeId::from_bytes([0x01; 20]),
        }
    }

    /// Boot a `Dht` with explicit channel halves so the test can
    /// observe outbound datagrams + inject inbound ones.
    fn boot_harness() -> Harness {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(32);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Datagram>(32);
        let (queries_tx, queries_rx) = mpsc::channel::<InboundQuery>(32);
        let (dht, join) = Dht::spawn(
            DhtConfig {
                query_timeout: Duration::from_millis(200),
                ..DhtConfig::default()
            },
            inbound_rx,
            outbound_tx,
            queries_tx,
        );
        Harness {
            dht,
            inbound_tx,
            outbound_rx,
            queries_rx,
            join,
        }
    }

    struct Harness {
        dht: Dht,
        inbound_tx: mpsc::Sender<Datagram>,
        outbound_rx: mpsc::Receiver<Datagram>,
        queries_rx: mpsc::Receiver<InboundQuery>,
        join: JoinHandle<()>,
    }

    #[tokio::test]
    async fn send_query_roundtrip_pumps_response() {
        let mut h = boot_harness();
        let target = loopback(6881);
        let query = ping_query();

        // Fire the query without awaiting it — we need to snoop the
        // outbound datagram to extract the txid, then hand-craft a
        // reply back through `inbound_tx`.
        let dht = h.dht.clone();
        let send = tokio::spawn(async move { dht.send_query(target, query).await });

        // Grab the outbound datagram the Dht emits.
        let outbound = h.outbound_rx.recv().await.expect("outbound datagram");
        assert_eq!(outbound.addr, target);
        let out_msg = KrpcMessage::decode(&outbound.data).unwrap();
        assert_eq!(out_msg.transaction_id.len(), 2);
        assert!(matches!(out_msg.kind, KrpcKind::Query(Query::Ping { .. })));

        // Build a fake response with the same txid.
        let reply = KrpcMessage {
            transaction_id: out_msg.transaction_id.clone(),
            kind: KrpcKind::Response(Response {
                id: NodeId::from_bytes([0xaa; 20]),
                ..Default::default()
            }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: reply.encode(),
                addr: target,
            })
            .await
            .unwrap();

        let response = send.await.unwrap().expect("query resolved");
        assert_eq!(response.id, NodeId::from_bytes([0xaa; 20]));
    }

    #[tokio::test]
    async fn send_query_times_out_when_no_response() {
        let h = boot_harness();
        let err = h
            .dht
            .send_query(loopback(6881), ping_query())
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::Timeout));
        assert_eq!(h.dht.pending_query_count().await, 0);
    }

    #[tokio::test]
    async fn send_query_rejects_on_remote_error_reply() {
        let mut h = boot_harness();
        let target = loopback(6881);
        let dht = h.dht.clone();
        let send = tokio::spawn(async move { dht.send_query(target, ping_query()).await });

        let outbound = h.outbound_rx.recv().await.unwrap();
        let txid = KrpcMessage::decode(&outbound.data).unwrap().transaction_id;

        let err_reply = KrpcMessage {
            transaction_id: txid,
            kind: KrpcKind::Error(KrpcErrorPayload {
                code: KrpcErrorPayload::CODE_PROTOCOL,
                message: "nope".into(),
            }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: err_reply.encode(),
                addr: target,
            })
            .await
            .unwrap();

        let err = send.await.unwrap().unwrap_err();
        assert!(matches!(err, QueryError::Remote { code: 203, .. }));
    }

    #[tokio::test]
    async fn outbound_closed_surfaces_as_error() {
        // Build manually so we can drop outbound_rx before querying.
        let (_inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(8);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Datagram>(8);
        let (queries_tx, _queries_rx) = mpsc::channel::<InboundQuery>(8);
        let (dht, _join) = Dht::spawn(DhtConfig::default(), inbound_rx, outbound_tx, queries_tx);
        drop(outbound_rx); // simulate send-loop shutdown

        let err = dht
            .send_query(loopback(6881), ping_query())
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::OutboundClosed));
        assert_eq!(dht.pending_query_count().await, 0);
    }

    #[tokio::test]
    async fn inbound_query_arrives_in_inbound_queries_channel() {
        let mut h = boot_harness();
        let remote = loopback(7000);

        let query_msg = KrpcMessage {
            transaction_id: b"xx".to_vec(),
            kind: KrpcKind::Query(Query::FindNode {
                id: NodeId::from_bytes([0x02; 20]),
                target: NodeId::from_bytes([0x03; 20]),
            }),
            client_version: Some(b"MP01".to_vec()),
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: query_msg.encode(),
                addr: remote,
            })
            .await
            .unwrap();

        let q = h.queries_rx.recv().await.expect("query delivered");
        assert_eq!(q.transaction_id, b"xx");
        assert_eq!(q.from, remote);
        assert_eq!(q.client_version.as_deref(), Some(&b"MP01"[..]));
        assert!(matches!(q.query, Query::FindNode { .. }));
    }

    #[tokio::test]
    async fn malformed_datagram_silently_dropped() {
        let mut h = boot_harness();
        h.inbound_tx
            .send(Datagram {
                data: b"not bencode".to_vec(),
                addr: loopback(7000),
            })
            .await
            .unwrap();
        // Give the pump a tick to process.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Nothing arrived on the queries channel.
        assert!(h.queries_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reply_with_wrong_txid_length_ignored() {
        let mut h = boot_harness();
        let target = loopback(6881);
        let dht = h.dht.clone();
        let send = tokio::spawn(async move { dht.send_query(target, ping_query()).await });

        let _outbound = h.outbound_rx.recv().await.unwrap();

        // Inject a response whose txid is 4 bytes — impossible match.
        let fake = KrpcMessage {
            transaction_id: vec![0x00, 0x00, 0x00, 0x00],
            kind: KrpcKind::Response(Response {
                id: NodeId::from_bytes([0xaa; 20]),
                ..Default::default()
            }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: fake.encode(),
                addr: target,
            })
            .await
            .unwrap();

        // send_query should time out, not resolve.
        let err = send.await.unwrap().unwrap_err();
        assert!(matches!(err, QueryError::Timeout));
    }

    #[tokio::test]
    async fn reply_with_unknown_txid_drops_silently() {
        let h = boot_harness();
        // Inject a response for a txid we never registered. Should
        // be dropped without panic.
        let stray = KrpcMessage {
            transaction_id: vec![0xff, 0xee],
            kind: KrpcKind::Response(Response {
                id: NodeId::from_bytes([0xaa; 20]),
                ..Default::default()
            }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: stray.encode(),
                addr: loopback(7000),
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(h.dht.pending_query_count().await, 0);
    }

    #[tokio::test]
    async fn inbound_close_shuts_down_pump_and_wakes_pending() {
        let mut h = boot_harness();
        let target = loopback(6881);
        let dht = h.dht.clone();
        let send = tokio::spawn(async move { dht.send_query(target, ping_query()).await });

        // Drain the outbound datagram (so the send has fully taken
        // the slot), then close inbound.
        let _ = h.outbound_rx.recv().await.unwrap();
        drop(h.inbound_tx);

        let err = send.await.unwrap().unwrap_err();
        assert!(matches!(err, QueryError::Shutdown | QueryError::Timeout));
        // The pump should exit.
        h.join.await.unwrap();
    }

    #[tokio::test]
    async fn txid_advances_across_queries() {
        let mut h = boot_harness();
        let target = loopback(6881);

        // Fire two queries; snoop the outbound txids; they should
        // differ (counter advances).
        let dht1 = h.dht.clone();
        let dht2 = h.dht.clone();
        let _j1 = tokio::spawn(async move { dht1.send_query(target, ping_query()).await });
        let _j2 = tokio::spawn(async move { dht2.send_query(target, ping_query()).await });

        let d1 = h.outbound_rx.recv().await.unwrap();
        let d2 = h.outbound_rx.recv().await.unwrap();
        let t1 = KrpcMessage::decode(&d1.data).unwrap().transaction_id;
        let t2 = KrpcMessage::decode(&d2.data).unwrap().transaction_id;
        assert_ne!(t1, t2);
    }

    #[tokio::test]
    async fn respond_emits_matching_txid_on_outbound() {
        let mut h = boot_harness();
        let txid = b"zz".to_vec();
        h.dht
            .respond(
                txid.clone(),
                ResponseKind::Response(Response {
                    id: NodeId::from_bytes([0xbb; 20]),
                    ..Default::default()
                }),
                loopback(7000),
            )
            .await
            .unwrap();

        let sent = h.outbound_rx.recv().await.unwrap();
        let decoded = KrpcMessage::decode(&sent.data).unwrap();
        assert_eq!(decoded.transaction_id, txid);
        assert_eq!(sent.addr, loopback(7000));
    }
}
