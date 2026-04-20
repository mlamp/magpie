//! Mainline DHT (BEP 5) for the magpie BitTorrent library.
//!
//! This crate is the home of magpie's Kademlia DHT: node IDs,
//! routing table, KRPC wire codec, bootstrap, tokens, BEP 42 salt,
//! and the `Dht` task that owns a shared UDP socket via
//! `magpie-bt-core`'s `UdpDemux`. It can be consumed directly for
//! raw DHT use, or via `magpie-bt-core`'s `dht` feature which wires
//! it into the engine's peer-source pipeline.
//!
//! # Scope in M4 workstream A
//!
//! This workstream ships the load-bearing data model and the wire
//! codec only:
//!
//! - [`NodeId`] with XOR distance ([`Distance`]).
//! - [`generate_node_id`] + [`validate_bep42`] for BEP 42 salting.
//! - [`Node`], [`Bucket`], and the routing-table constants (`K`,
//!   quality-state thresholds).
//! - [`KrpcMessage`], [`Query`], [`Response`], [`KrpcErrorPayload`]
//!   with bencode encode/decode and a structured [`KrpcError`] type.
//!
//! Transport integration, RPC handlers, bootstrap, tokens, and rate
//! limits land in workstreams B–G.
//!
//! # Example
//!
//! ```
//! use magpie_bt_dht::{KrpcMessage, KrpcKind, Query, NodeId};
//!
//! let msg = KrpcMessage {
//!     transaction_id: b"aa".to_vec(),
//!     kind: KrpcKind::Query(Query::Ping {
//!         id: NodeId::from_bytes([0x01; 20]),
//!     }),
//!     client_version: None,
//!     ip: None,
//! };
//! let bytes = msg.encode();
//! let back = KrpcMessage::decode(&bytes).unwrap();
//! assert_eq!(back, msg);
//! ```
#![forbid(unsafe_code)]

mod bootstrap;
mod bucket;
mod handlers;
mod krpc;
mod lookup;
mod node_id;
mod peer_store;
mod rate_limit;
mod routing_table;
mod runtime;
mod tokens;
mod transport;

pub use bootstrap::{
    BootstrapConfig, BootstrapOutcome, DEFAULT_CONTACT_QUERY_TIMEOUT, DEFAULT_EXIT_GOOD_NODES,
    DEFAULT_PING_BATCH, DEFAULT_ROUND_INTERVAL, DEFAULT_STALL_AFTER, DEFAULT_STALL_THRESHOLD,
    DEFAULT_STALLED_INTERVAL, run_bootstrap,
};
pub use bucket::{
    BAD_REMOVE_AFTER, Bucket, K, MAX_CONSECUTIVE_FAILURES, Node, NodeQuality, QUESTIONABLE_AFTER,
};
pub use handlers::{DhtState, handle_query};
pub use krpc::{
    COMPACT_NODE_V4_LEN, COMPACT_NODE_V6_LEN, COMPACT_PEER_V4_LEN, COMPACT_PEER_V6_LEN,
    CompactNode, CompactNode6, INFO_HASH_LEN, InfoHash, KrpcError, KrpcErrorPayload, KrpcKind,
    KrpcMessage, MAX_CLIENT_VERSION_LEN, MAX_ERROR_MESSAGE_LEN, MAX_NODES_BYTES, MAX_TOKEN_LEN,
    MAX_TRANSACTION_ID_LEN, MAX_VALUES_PEERS, Query, Response,
};
pub use lookup::{
    ALPHA, GetPeersResult, MAX_LOOKUP_ROUNDS, announce_to_token_nodes, iterative_get_peers,
};
pub use node_id::{Distance, LocalNodeId, NodeId, generate_node_id, validate_bep42};
pub use peer_store::{
    DEFAULT_MAX_PEERS_PER_TORRENT, DEFAULT_MAX_TORRENTS, DEFAULT_PEER_STALENESS, PeerStore,
    PeerStoreConfig,
};
pub use rate_limit::{
    DEFAULT_BUCKET_IDLE, DEFAULT_INBOUND_GLOBAL_BURST, DEFAULT_INBOUND_GLOBAL_QPS,
    DEFAULT_INBOUND_PER_IP_BURST, DEFAULT_INBOUND_PER_IP_QPS, DEFAULT_OUTBOUND_PER_NODE_BURST,
    DEFAULT_OUTBOUND_PER_NODE_QPS, RateLimitConfig, RateLimiter,
};
pub use routing_table::{BUCKET_REFRESH_AFTER, Insertion, RoutingTable};
pub use runtime::{DhtRuntime, DhtRuntimeConfig, DhtRuntimeJoins};
pub use tokens::{TOKEN_LENGTH, TOKEN_ROTATION_INTERVAL, TokenSecrets};
pub use transport::{
    DEFAULT_INBOUND_QUERIES_CAPACITY, DEFAULT_OUTBOUND_CAPACITY, DEFAULT_QUERY_TIMEOUT, Datagram,
    Dht, DhtConfig, InboundQuery, MAX_PENDING_QUERIES, QueryError, ResponseKind,
};
