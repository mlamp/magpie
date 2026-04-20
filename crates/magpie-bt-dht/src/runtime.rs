//! Public `DhtRuntime` — the composed DHT: [`Dht`] transport pumps
//! plus the inbound-query handler loop plus rate limiting.
//!
//! This is the type a consumer holds to participate in the DHT.
//! The consumer owns both ends of the UDP transport (typically via
//! `magpie-bt-core`'s `UdpDemux`) and pipes datagrams in/out via
//! the mpsc channel pair [`DhtRuntime::spawn`] takes.
//!
//! What the runtime glues together:
//!
//! * [`Dht::spawn`] — decodes inbound datagrams, routes responses
//!   to pending queries, surfaces inbound queries on an internal
//!   channel.
//! * Handler loop — drains the inbound-queries channel, applies
//!   [`RateLimiter::check_inbound`], dispatches to [`handle_query`],
//!   and sends the response back via [`Dht::respond`].
//!
//! Iterative Kademlia lookups + `Dht::announce` + bootstrap live in
//! follow-up workstreams; this module delivers the scaffolding all
//! of them sit on.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::handlers::{DhtState, handle_query};
use crate::krpc::{InfoHash, Query, Response};
use crate::node_id::NodeId;
use crate::peer_store::{PeerStore, PeerStoreConfig};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::routing_table::RoutingTable;
use crate::tokens::TokenSecrets;
use crate::transport::{
    DEFAULT_INBOUND_QUERIES_CAPACITY, Datagram, Dht, DhtConfig, InboundQuery, QueryError,
    ResponseKind,
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Runtime knobs for a complete [`DhtRuntime`].
#[derive(Debug, Clone)]
pub struct DhtRuntimeConfig {
    /// Our own 160-bit id. Use
    /// [`LocalNodeId`](crate::LocalNodeId) to manage BEP 42
    /// two-phase derivation upstream and pass its current id here.
    pub local_id: NodeId,
    /// Transport-layer knobs ([`Dht::spawn`]).
    pub transport: DhtConfig,
    /// Inbound + outbound rate-limit tiers.
    pub rate_limits: RateLimitConfig,
    /// `announce_peer` / `get_peers` backing store knobs.
    pub peer_store: PeerStoreConfig,
}

impl DhtRuntimeConfig {
    /// Defaults for every field except `local_id`.
    #[must_use]
    pub fn new(local_id: NodeId) -> Self {
        Self {
            local_id,
            transport: DhtConfig::default(),
            rate_limits: RateLimitConfig::default(),
            peer_store: PeerStoreConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// DhtRuntime
// ---------------------------------------------------------------------------

/// The composed DHT handle. Cloneable; each clone shares state with
/// the originating spawn.
#[derive(Clone)]
pub struct DhtRuntime {
    dht: Dht,
    state: Arc<Mutex<DhtState>>,
    rate: Arc<Mutex<RateLimiter>>,
}

impl DhtRuntime {
    /// Wire up a complete DHT. `now` is the baseline for token
    /// rotation + rate-limit refills; typically `Instant::now()`.
    ///
    /// # Errors
    ///
    /// [`getrandom::Error`] from [`TokenSecrets::new`].
    pub fn spawn(
        cfg: DhtRuntimeConfig,
        inbound: mpsc::Receiver<Datagram>,
        outbound: mpsc::Sender<Datagram>,
        now: Instant,
    ) -> Result<(Self, DhtRuntimeJoins), getrandom::Error> {
        let tokens = TokenSecrets::new(now)?;
        let state = DhtState {
            local_id: cfg.local_id,
            routing: RoutingTable::new(cfg.local_id, now),
            peers: PeerStore::new(cfg.peer_store),
            tokens,
        };
        let state = Arc::new(Mutex::new(state));
        let rate = Arc::new(Mutex::new(RateLimiter::new(cfg.rate_limits, now)));

        let (queries_tx, queries_rx) = mpsc::channel::<InboundQuery>(
            cfg.transport
                .inbound_queries_capacity
                .max(DEFAULT_INBOUND_QUERIES_CAPACITY),
        );
        let (dht, pump_join) = Dht::spawn(cfg.transport, inbound, outbound, queries_tx);

        let handler_state = Arc::clone(&state);
        let handler_rate = Arc::clone(&rate);
        let handler_dht = dht.clone();
        let handler_join = tokio::spawn(async move {
            run_handler_loop(queries_rx, handler_state, handler_rate, handler_dht).await;
        });

        let joins = DhtRuntimeJoins {
            pump: pump_join,
            handler: handler_join,
        };
        Ok((Self { dht, state, rate }, joins))
    }

    /// The transport-side handle (for `send_query` and outbound
    /// query flow).
    #[must_use]
    pub const fn client(&self) -> &Dht {
        &self.dht
    }

    /// Our own node id.
    pub async fn local_id(&self) -> NodeId {
        self.state.lock().await.local_id
    }

    /// Send a raw query. Convenience forward to the inner `Dht`.
    ///
    /// # Errors
    ///
    /// See [`Dht::send_query`].
    pub async fn send_query(
        &self,
        target: SocketAddr,
        query: Query,
    ) -> Result<Response, QueryError> {
        self.dht.send_query(target, query).await
    }

    /// Routing-table node count snapshot.
    pub async fn node_count(&self) -> usize {
        self.state.lock().await.routing.node_count()
    }

    /// Good-node count snapshot (ADR-0025 exit criterion).
    pub async fn good_node_count(&self) -> usize {
        self.state.lock().await.routing.good_node_count()
    }

    /// Known peers for `info_hash` in the local peer store.
    pub async fn local_peers_for(&self, info_hash: &InfoHash, limit: usize) -> Vec<SocketAddr> {
        self.state.lock().await.peers.peers_for(info_hash, limit)
    }

    /// Counter — total inbound queries silently rate-limited.
    pub async fn dropped_inbound(&self) -> u64 {
        self.rate.lock().await.dropped_inbound()
    }

    /// Insert a known contact directly into the routing table — the
    /// bootstrap controller calls this for cache-loaded + consumer-
    /// supplied contacts before the first ping round.
    pub async fn seed_contact(&self, id: NodeId, addr: SocketAddr, now: Instant) {
        let mut state = self.state.lock().await;
        state.routing.insert(id, addr, now);
    }
}

impl std::fmt::Debug for DhtRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtRuntime").finish_non_exhaustive()
    }
}

/// `JoinHandle` pair returned by [`DhtRuntime::spawn`]. Drop to
/// detach; await to run to completion when the channels close.
#[derive(Debug)]
pub struct DhtRuntimeJoins {
    /// Inbound datagram decode pump.
    pub pump: JoinHandle<()>,
    /// Inbound-query handler dispatch loop.
    pub handler: JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// Dispatch loop
// ---------------------------------------------------------------------------

async fn run_handler_loop(
    mut queries_rx: mpsc::Receiver<InboundQuery>,
    state: Arc<Mutex<DhtState>>,
    rate: Arc<Mutex<RateLimiter>>,
    dht: Dht,
) {
    while let Some(query) = queries_rx.recv().await {
        let now = Instant::now();
        if !rate_accept(&rate, query.from.ip(), now).await {
            // Silent drop per ADR-0026; counter already incremented.
            continue;
        }
        let outcome = {
            let mut state = state.lock().await;
            handle_query(&mut state, &query.query, query.from, now)
        };
        let kind = match outcome {
            Ok(r) => ResponseKind::Response(r),
            Err(e) => ResponseKind::Error(e),
        };
        if dht
            .respond(query.transaction_id, kind, query.from)
            .await
            .is_err()
        {
            // Outbound closed — the whole runtime is shutting down.
            break;
        }
    }
}

async fn rate_accept(rate: &Arc<Mutex<RateLimiter>>, ip: IpAddr, now: Instant) -> bool {
    rate.lock().await.check_inbound(ip, now)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Duration;

    use crate::krpc::{KrpcKind, KrpcMessage, Query as KrpcQuery};

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    fn nid(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 20])
    }

    struct Harness {
        runtime: DhtRuntime,
        inbound_tx: mpsc::Sender<Datagram>,
        outbound_rx: mpsc::Receiver<Datagram>,
        _joins: DhtRuntimeJoins,
    }

    fn boot() -> Harness {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(32);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Datagram>(32);
        let cfg = DhtRuntimeConfig::new(nid(0x01));
        let (runtime, joins) =
            DhtRuntime::spawn(cfg, inbound_rx, outbound_tx, Instant::now()).unwrap();
        Harness {
            runtime,
            inbound_tx,
            outbound_rx,
            _joins: joins,
        }
    }

    #[tokio::test]
    async fn inbound_ping_emits_response_with_local_id() {
        let mut h = boot();
        let ping = KrpcMessage {
            transaction_id: b"tt".to_vec(),
            kind: KrpcKind::Query(KrpcQuery::Ping { id: nid(0x42) }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: ping.encode(),
                addr: v4(10, 0, 0, 5, 6881),
            })
            .await
            .unwrap();

        let out = tokio::time::timeout(Duration::from_secs(2), h.outbound_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.addr, v4(10, 0, 0, 5, 6881));
        let decoded = KrpcMessage::decode(&out.data).unwrap();
        let KrpcKind::Response(r) = decoded.kind else {
            panic!("not a response");
        };
        assert_eq!(r.id, nid(0x01));
        assert_eq!(decoded.transaction_id, b"tt");
        // Sender was folded into the routing table.
        assert_eq!(h.runtime.node_count().await, 1);
    }

    #[tokio::test]
    async fn inbound_announce_peer_bad_token_returns_203() {
        let mut h = boot();
        let announce = KrpcMessage {
            transaction_id: b"ap".to_vec(),
            kind: KrpcKind::Query(KrpcQuery::AnnouncePeer {
                id: nid(0x42),
                info_hash: InfoHash::from_bytes([0xab; 20]),
                port: 6881,
                implied_port: false,
                token: vec![0u8; 8],
            }),
            client_version: None,
            ip: None,
        };
        h.inbound_tx
            .send(Datagram {
                data: announce.encode(),
                addr: v4(10, 0, 0, 5, 6881),
            })
            .await
            .unwrap();

        let out = tokio::time::timeout(Duration::from_secs(2), h.outbound_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let decoded = KrpcMessage::decode(&out.data).unwrap();
        let KrpcKind::Error(e) = decoded.kind else {
            panic!("expected error reply");
        };
        assert_eq!(e.code, 203);
    }

    #[tokio::test]
    async fn rate_limited_inbound_silently_dropped() {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(2048);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Datagram>(2048);
        // Very tight per-IP cap to make the flood test fast.
        let mut cfg = DhtRuntimeConfig::new(nid(0x01));
        cfg.rate_limits.inbound_per_ip_burst = 2;
        cfg.rate_limits.inbound_per_ip_qps = 1;
        let (runtime, _joins) =
            DhtRuntime::spawn(cfg, inbound_rx, outbound_tx, Instant::now()).unwrap();

        let addr = v4(10, 0, 0, 5, 6881);
        // Queue 20 pings from the same IP — bucket admits 2, drops 18.
        for i in 0..20u8 {
            let ping = KrpcMessage {
                transaction_id: vec![i, 0],
                kind: KrpcKind::Query(KrpcQuery::Ping { id: nid(0x42) }),
                client_version: None,
                ip: None,
            };
            inbound_tx
                .send(Datagram {
                    data: ping.encode(),
                    addr,
                })
                .await
                .unwrap();
        }

        let mut responses = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while responses < 2 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, outbound_rx.recv()).await {
                Ok(Some(_)) => responses += 1,
                _ => break,
            }
        }
        assert_eq!(responses, 2);
        // Give the loop a moment to increment the drop counter for
        // the trailing queries.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(runtime.dropped_inbound().await >= 10);
    }

    #[tokio::test]
    async fn seed_contact_populates_routing_table() {
        let h = boot();
        h.runtime
            .seed_contact(nid(0x99), v4(10, 0, 0, 9, 6881), Instant::now())
            .await;
        assert_eq!(h.runtime.node_count().await, 1);
    }
}
