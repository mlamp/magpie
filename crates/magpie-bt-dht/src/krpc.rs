//! KRPC wire codec — BEP 5 message layer.
//!
//! KRPC is the four-query bencoded RPC layered on UDP that drives the
//! Mainline DHT. This module owns the structured message types
//! ([`KrpcMessage`], [`Query`], [`Response`], [`KrpcErrorPayload`])
//! plus the encode/decode round-trip between them and the bencode
//! byte representation. Transport (the `Dht` task pumping datagrams
//! through `magpie-bt-core`'s `UdpDemux`) lives elsewhere.
//!
//! Parsing is strict at the structural level (typed [`KrpcError`] on
//! malformed input) and defensive on size (per-field caps bound
//! decoder allocation against hostile datagrams — see `MAX_*`
//! constants). Forward-compatible on unknown scalar fields: anything
//! the spec doesn't define is silently ignored.
//!
//! BEP 5 — <http://www.bittorrent.org/beps/bep_0005.html>.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use thiserror::Error;

use magpie_bt_bencode::{Value, decode, encode};

use crate::NodeId;

// ---------------------------------------------------------------------------
// Capacity caps (hostile-input defence)
// ---------------------------------------------------------------------------

/// BitTorrent info-hash byte length. Fixed at 20 on the wire; v2
/// hybrid torrents use the truncated v1 hash for DHT lookups, so this
/// stays 20 independently of BEP 52.
pub const INFO_HASH_LEN: usize = 20;

/// Per-node compact representation: 20-byte id + 4-byte v4 + 2-byte port.
pub const COMPACT_NODE_V4_LEN: usize = 20 + 4 + 2;

/// Per-node compact representation: 20-byte id + 16-byte v6 + 2-byte port (BEP 32).
pub const COMPACT_NODE_V6_LEN: usize = 20 + 16 + 2;

/// Per-peer compact representation: 4-byte v4 + 2-byte port.
pub const COMPACT_PEER_V4_LEN: usize = 4 + 2;

/// Per-peer compact representation: 16-byte v6 + 2-byte port (BEP 32).
pub const COMPACT_PEER_V6_LEN: usize = 16 + 2;

/// Transaction-id byte ceiling. Rakshasa mints 2-byte ids; BEP 5 is
/// silent so we accept more on inbound but cap to reject obvious
/// amplification payloads.
pub const MAX_TRANSACTION_ID_LEN: usize = 32;

/// Maximum bytes in an `announce_peer` / `get_peers` token.
/// Our own tokens are 8 bytes (ADR-0026); allow more on inbound for
/// peers that use longer tokens.
pub const MAX_TOKEN_LEN: usize = 64;

/// Maximum compact-node payload in a single response (bytes).
/// Sized for 8 × [`COMPACT_NODE_V6_LEN`] so a pathological v6 reply
/// is still bounded but generous.
pub const MAX_NODES_BYTES: usize = 8 * COMPACT_NODE_V6_LEN;

/// Maximum `values` peer-list size (entries). Bounds decoder
/// allocation on a `get_peers` response.
pub const MAX_VALUES_PEERS: usize = 200;

/// Maximum client `v` string length (octets).
pub const MAX_CLIENT_VERSION_LEN: usize = 32;

/// Maximum KRPC error message length (octets).
pub const MAX_ERROR_MESSAGE_LEN: usize = 256;

// ---------------------------------------------------------------------------
// Info hash
// ---------------------------------------------------------------------------

/// BEP 3 torrent info-hash as carried on the wire.
///
/// Stored as raw bytes rather than reusing a crate-wide alias so
/// `magpie-bt-dht` does not pull `magpie-bt-metainfo` in for a
/// 20-byte type. The forthcoming `magpie_bt_core` glue converts to
/// whatever the engine owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHash([u8; INFO_HASH_LEN]);

impl InfoHash {
    /// Wrap 20 raw bytes as an info-hash.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; INFO_HASH_LEN]) -> Self {
        Self(bytes)
    }

    /// Raw bytes in wire order.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; INFO_HASH_LEN] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Compact nodes
// ---------------------------------------------------------------------------

/// A (node-id, ipv4-addr) pair in the 26-byte compact layout used by
/// BEP 5 `find_node` / `get_peers` responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactNode {
    /// Remote node id.
    pub id: NodeId,
    /// Remote UDP address.
    pub addr: SocketAddrV4,
}

impl CompactNode {
    /// Serialise into 26 big-endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; COMPACT_NODE_V4_LEN] {
        let mut out = [0u8; COMPACT_NODE_V4_LEN];
        out[..20].copy_from_slice(self.id.as_bytes());
        out[20..24].copy_from_slice(&self.addr.ip().octets());
        out[24..26].copy_from_slice(&self.addr.port().to_be_bytes());
        out
    }

    /// Parse 26 bytes into a compact node. Returns `None` when the
    /// input is the wrong length.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != COMPACT_NODE_V4_LEN {
            return None;
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(&bytes[..20]);
        let ip = Ipv4Addr::new(bytes[20], bytes[21], bytes[22], bytes[23]);
        let port = u16::from_be_bytes([bytes[24], bytes[25]]);
        Some(Self {
            id: NodeId::from_bytes(id),
            addr: SocketAddrV4::new(ip, port),
        })
    }
}

/// A (node-id, ipv6-addr) pair in the 38-byte compact layout used by
/// BEP 32 `nodes6` responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactNode6 {
    /// Remote node id.
    pub id: NodeId,
    /// Remote UDP address.
    pub addr: SocketAddrV6,
}

impl CompactNode6 {
    /// Serialise into 38 big-endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; COMPACT_NODE_V6_LEN] {
        let mut out = [0u8; COMPACT_NODE_V6_LEN];
        out[..20].copy_from_slice(self.id.as_bytes());
        out[20..36].copy_from_slice(&self.addr.ip().octets());
        out[36..38].copy_from_slice(&self.addr.port().to_be_bytes());
        out
    }

    /// Parse 38 bytes into a compact v6 node. Returns `None` when the
    /// input is the wrong length.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != COMPACT_NODE_V6_LEN {
            return None;
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(&bytes[..20]);
        let mut ip = [0u8; 16];
        ip.copy_from_slice(&bytes[20..36]);
        let port = u16::from_be_bytes([bytes[36], bytes[37]]);
        Some(Self {
            id: NodeId::from_bytes(id),
            addr: SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0),
        })
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures decoding or encoding a KRPC message.
#[derive(Debug, Error)]
pub enum KrpcError {
    /// The bencode payload could not be parsed or has the wrong root.
    #[error("bencode decode error: {0}")]
    Decode(String),
    /// A required field is missing.
    #[error("missing field {0:?}")]
    MissingField(&'static str),
    /// A field has the wrong bencode type.
    #[error("field {field:?} has wrong type: {reason}")]
    WrongFieldType {
        /// Offending field key.
        field: &'static str,
        /// Short description of what was expected.
        reason: &'static str,
    },
    /// The `y` field was not `q`, `r`, or `e`.
    #[error("unknown message type {0:?}")]
    UnknownMessageType(String),
    /// The `q` method name is not one of the four BEP 5 queries.
    #[error("unknown query method {0:?}")]
    UnknownQueryMethod(String),
    /// A fixed-width field (node id, info-hash) has the wrong length.
    #[error("field {field:?} expected {expected} bytes, got {actual}")]
    InvalidLength {
        /// Offending field key.
        field: &'static str,
        /// Expected byte length.
        expected: usize,
        /// Actual byte length supplied.
        actual: usize,
    },
    /// A byte string exceeded its defensive cap.
    #[error("field {field:?} oversize: {actual} > {max}")]
    Oversize {
        /// Offending field key.
        field: &'static str,
        /// Declared byte length.
        actual: usize,
        /// Configured ceiling.
        max: usize,
    },
    /// Compact-node list length is not a multiple of 26 or 38.
    #[error("compact {which} list length {len} not a multiple of {stride}")]
    MisalignedCompactList {
        /// Which list — `"nodes"` or `"nodes6"`.
        which: &'static str,
        /// Actual byte length.
        len: usize,
        /// Per-entry stride expected.
        stride: usize,
    },
    /// Port was `0` where the spec requires it to be reachable.
    #[error("field {0:?} has invalid port 0")]
    ZeroPort(&'static str),
    /// A numeric field was outside its domain (e.g. negative port).
    #[error("field {field:?} out of range: {value}")]
    OutOfRange {
        /// Offending field key.
        field: &'static str,
        /// Raw integer value.
        value: i64,
    },
}

// ---------------------------------------------------------------------------
// KRPC error payload (separate from KrpcError — the former is the wire
// `e` list, the latter is our decode/encode failure type)
// ---------------------------------------------------------------------------

/// Payload of a KRPC `y = "e"` message.
///
/// BEP 5 error codes:
/// - `201` generic error
/// - `202` server error
/// - `203` protocol error
/// - `204` method unknown
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcErrorPayload {
    /// Numeric error code.
    pub code: i32,
    /// Short UTF-8 description.
    pub message: String,
}

impl KrpcErrorPayload {
    /// 201 Generic Error — BEP 5.
    pub const CODE_GENERIC: i32 = 201;
    /// 202 Server Error — BEP 5.
    pub const CODE_SERVER: i32 = 202;
    /// 203 Protocol Error — BEP 5.
    pub const CODE_PROTOCOL: i32 = 203;
    /// 204 Method Unknown — BEP 5.
    pub const CODE_METHOD_UNKNOWN: i32 = 204;
}

// ---------------------------------------------------------------------------
// Query / Response
// ---------------------------------------------------------------------------

/// A BEP 5 query body, keyed by method name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `ping`: liveness probe.
    Ping {
        /// Sender's node id.
        id: NodeId,
    },
    /// `find_node`: request the 8 nodes closest to `target`.
    FindNode {
        /// Sender's node id.
        id: NodeId,
        /// Target node id being searched for.
        target: NodeId,
    },
    /// `get_peers`: ask for peers for `info_hash`, or closest nodes.
    GetPeers {
        /// Sender's node id.
        id: NodeId,
        /// Torrent info-hash.
        info_hash: InfoHash,
    },
    /// `announce_peer`: record the sender as a peer for `info_hash`.
    AnnouncePeer {
        /// Sender's node id.
        id: NodeId,
        /// Torrent info-hash.
        info_hash: InfoHash,
        /// Sender's listening port (ignored if `implied_port`).
        port: u16,
        /// When `true`, receiver uses the datagram source port.
        implied_port: bool,
        /// Token echoed from a prior `get_peers` reply.
        token: Vec<u8>,
    },
}

impl Query {
    /// BEP 5 method name (`q` field) for this variant.
    #[must_use]
    pub const fn method_name(&self) -> &'static str {
        match self {
            Self::Ping { .. } => "ping",
            Self::FindNode { .. } => "find_node",
            Self::GetPeers { .. } => "get_peers",
            Self::AnnouncePeer { .. } => "announce_peer",
        }
    }
}

/// A BEP 5 response body (the `r` dictionary).
///
/// All four queries share this shape because response keys are
/// conditionally present: `ping` populates only `id`, `find_node`
/// populates `id` + `nodes`, `get_peers` populates `id` + `token` +
/// exactly one of `nodes` / `values`, `announce_peer` populates only
/// `id`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Response {
    /// Responder's node id. Always present — BEP 5.
    pub id: NodeId,
    /// Compact v4 nodes (BEP 5 `nodes`).
    pub nodes: Vec<CompactNode>,
    /// Compact v6 nodes (BEP 32 `nodes6`).
    pub nodes6: Vec<CompactNode6>,
    /// `values` peers for a `get_peers` hit.
    pub values: Vec<SocketAddr>,
    /// `token` returned from `get_peers` to gate `announce_peer`.
    pub token: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// KrpcMessage
// ---------------------------------------------------------------------------

/// A complete KRPC datagram, ready to encode / just decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcMessage {
    /// Transaction id; echoed by replies. Typically 2 bytes for ours.
    pub transaction_id: Vec<u8>,
    /// Query, response, or error body.
    pub kind: KrpcKind,
    /// Optional client `v` string (e.g. `b"MP01"`).
    pub client_version: Option<Vec<u8>>,
    /// Optional BEP 42 `ip` echo — what the remote saw as our source.
    pub ip: Option<SocketAddr>,
}

/// Body of a [`KrpcMessage`] — the three BEP 5 top-level dispositions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcKind {
    /// `y = "q"`: an inbound query.
    Query(Query),
    /// `y = "r"`: a response to one of our queries.
    Response(Response),
    /// `y = "e"`: an error reply.
    Error(KrpcErrorPayload),
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

impl KrpcMessage {
    /// Bencode-encode this message into a fresh `Vec<u8>`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"t"),
            Value::Bytes(Cow::Owned(self.transaction_id.clone())),
        );

        match &self.kind {
            KrpcKind::Query(q) => {
                dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
                dict.insert(
                    Cow::Borrowed(b"q"),
                    Value::Bytes(Cow::Owned(q.method_name().as_bytes().to_vec())),
                );
                dict.insert(Cow::Borrowed(b"a"), encode_query_args(q));
            }
            KrpcKind::Response(r) => {
                dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"r")));
                dict.insert(Cow::Borrowed(b"r"), encode_response(r));
            }
            KrpcKind::Error(e) => {
                dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"e")));
                dict.insert(
                    Cow::Borrowed(b"e"),
                    Value::List(vec![
                        Value::Int(i64::from(e.code)),
                        Value::Bytes(Cow::Owned(e.message.as_bytes().to_vec())),
                    ]),
                );
            }
        }

        if let Some(v) = &self.client_version {
            dict.insert(Cow::Borrowed(b"v"), Value::Bytes(Cow::Owned(v.clone())));
        }

        if let Some(ip) = &self.ip {
            dict.insert(
                Cow::Borrowed(b"ip"),
                Value::Bytes(Cow::Owned(encode_compact_addr(ip))),
            );
        }

        encode(&Value::Dict(dict))
    }
}

fn encode_query_args(q: &Query) -> Value<'static> {
    let mut args: BTreeMap<Cow<'static, [u8]>, Value<'static>> = BTreeMap::new();
    match q {
        Query::Ping { id } => {
            args.insert(
                Cow::Borrowed(b"id"),
                Value::Bytes(Cow::Owned(id.as_bytes().to_vec())),
            );
        }
        Query::FindNode { id, target } => {
            args.insert(
                Cow::Borrowed(b"id"),
                Value::Bytes(Cow::Owned(id.as_bytes().to_vec())),
            );
            args.insert(
                Cow::Borrowed(b"target"),
                Value::Bytes(Cow::Owned(target.as_bytes().to_vec())),
            );
        }
        Query::GetPeers { id, info_hash } => {
            args.insert(
                Cow::Borrowed(b"id"),
                Value::Bytes(Cow::Owned(id.as_bytes().to_vec())),
            );
            args.insert(
                Cow::Borrowed(b"info_hash"),
                Value::Bytes(Cow::Owned(info_hash.as_bytes().to_vec())),
            );
        }
        Query::AnnouncePeer {
            id,
            info_hash,
            port,
            implied_port,
            token,
        } => {
            args.insert(
                Cow::Borrowed(b"id"),
                Value::Bytes(Cow::Owned(id.as_bytes().to_vec())),
            );
            if *implied_port {
                args.insert(Cow::Borrowed(b"implied_port"), Value::Int(1));
            }
            args.insert(
                Cow::Borrowed(b"info_hash"),
                Value::Bytes(Cow::Owned(info_hash.as_bytes().to_vec())),
            );
            args.insert(Cow::Borrowed(b"port"), Value::Int(i64::from(*port)));
            args.insert(
                Cow::Borrowed(b"token"),
                Value::Bytes(Cow::Owned(token.clone())),
            );
        }
    }
    Value::Dict(args)
}

fn encode_response(r: &Response) -> Value<'static> {
    let mut out: BTreeMap<Cow<'static, [u8]>, Value<'static>> = BTreeMap::new();
    out.insert(
        Cow::Borrowed(b"id"),
        Value::Bytes(Cow::Owned(r.id.as_bytes().to_vec())),
    );

    if !r.nodes.is_empty() {
        let mut buf = Vec::with_capacity(r.nodes.len() * COMPACT_NODE_V4_LEN);
        for n in &r.nodes {
            buf.extend_from_slice(&n.to_bytes());
        }
        out.insert(Cow::Borrowed(b"nodes"), Value::Bytes(Cow::Owned(buf)));
    }

    if !r.nodes6.is_empty() {
        let mut buf = Vec::with_capacity(r.nodes6.len() * COMPACT_NODE_V6_LEN);
        for n in &r.nodes6 {
            buf.extend_from_slice(&n.to_bytes());
        }
        out.insert(Cow::Borrowed(b"nodes6"), Value::Bytes(Cow::Owned(buf)));
    }

    if !r.values.is_empty() {
        let values: Vec<Value<'static>> = r
            .values
            .iter()
            .map(|a| Value::Bytes(Cow::Owned(encode_compact_addr(a))))
            .collect();
        out.insert(Cow::Borrowed(b"values"), Value::List(values));
    }

    if let Some(token) = &r.token {
        out.insert(
            Cow::Borrowed(b"token"),
            Value::Bytes(Cow::Owned(token.clone())),
        );
    }

    Value::Dict(out)
}

fn encode_compact_addr(addr: &SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut out = Vec::with_capacity(COMPACT_PEER_V4_LEN);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
            out
        }
        SocketAddr::V6(v6) => {
            let mut out = Vec::with_capacity(COMPACT_PEER_V6_LEN);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

impl KrpcMessage {
    /// Decode a bencoded KRPC datagram.
    ///
    /// # Errors
    ///
    /// [`KrpcError`] for malformed structure, missing required
    /// fields, or any cap exceeded (see `MAX_*` constants).
    pub fn decode(data: &[u8]) -> Result<Self, KrpcError> {
        let val = decode(data).map_err(|e| KrpcError::Decode(e.to_string()))?;
        let dict = val
            .as_dict()
            .ok_or_else(|| KrpcError::Decode("top-level is not a dict".into()))?;

        let transaction_id = bytes_field(dict, b"t", "t")?.to_vec();
        if transaction_id.len() > MAX_TRANSACTION_ID_LEN {
            return Err(KrpcError::Oversize {
                field: "t",
                actual: transaction_id.len(),
                max: MAX_TRANSACTION_ID_LEN,
            });
        }

        let y = bytes_field(dict, b"y", "y")?;
        let kind = match y {
            b"q" => {
                let method = bytes_field(dict, b"q", "q")?.to_vec();
                let args = dict
                    .get(b"a".as_slice())
                    .ok_or(KrpcError::MissingField("a"))?;
                let args_dict = args.as_dict().ok_or(KrpcError::WrongFieldType {
                    field: "a",
                    reason: "not a dict",
                })?;
                KrpcKind::Query(decode_query(&method, args_dict)?)
            }
            b"r" => {
                let r = dict
                    .get(b"r".as_slice())
                    .ok_or(KrpcError::MissingField("r"))?;
                let r_dict = r.as_dict().ok_or(KrpcError::WrongFieldType {
                    field: "r",
                    reason: "not a dict",
                })?;
                KrpcKind::Response(decode_response(r_dict)?)
            }
            b"e" => {
                let e = dict
                    .get(b"e".as_slice())
                    .ok_or(KrpcError::MissingField("e"))?;
                KrpcKind::Error(decode_error_payload(e)?)
            }
            other => {
                return Err(KrpcError::UnknownMessageType(
                    String::from_utf8_lossy(other).into_owned(),
                ));
            }
        };

        let client_version = dict
            .get(b"v".as_slice())
            .and_then(Value::as_bytes)
            .map(<[u8]>::to_vec);
        if let Some(v) = &client_version
            && v.len() > MAX_CLIENT_VERSION_LEN
        {
            return Err(KrpcError::Oversize {
                field: "v",
                actual: v.len(),
                max: MAX_CLIENT_VERSION_LEN,
            });
        }

        let ip = if let Some(ip_val) = dict.get(b"ip".as_slice()) {
            Some(decode_compact_addr(
                ip_val.as_bytes().ok_or(KrpcError::WrongFieldType {
                    field: "ip",
                    reason: "not bytes",
                })?,
                "ip",
            )?)
        } else {
            None
        };

        Ok(Self {
            transaction_id,
            kind,
            client_version,
            ip,
        })
    }
}

fn decode_query(
    method: &[u8],
    args: &BTreeMap<Cow<'_, [u8]>, Value<'_>>,
) -> Result<Query, KrpcError> {
    let id = node_id_field(args, b"id", "a.id")?;
    match method {
        b"ping" => Ok(Query::Ping { id }),
        b"find_node" => {
            let target = node_id_field(args, b"target", "a.target")?;
            Ok(Query::FindNode { id, target })
        }
        b"get_peers" => {
            let info_hash = info_hash_field(args, b"info_hash", "a.info_hash")?;
            Ok(Query::GetPeers { id, info_hash })
        }
        b"announce_peer" => {
            let info_hash = info_hash_field(args, b"info_hash", "a.info_hash")?;
            let port_i64 = int_field(args, b"port", "a.port")?;
            let port = u16::try_from(port_i64).map_err(|_| KrpcError::OutOfRange {
                field: "a.port",
                value: port_i64,
            })?;
            let token = bytes_owned_field(args, b"token", "a.token", MAX_TOKEN_LEN)?;
            let implied_port = args
                .get(b"implied_port".as_slice())
                .and_then(Value::as_int)
                .is_some_and(|v| v != 0);
            Ok(Query::AnnouncePeer {
                id,
                info_hash,
                port,
                implied_port,
                token,
            })
        }
        other => Err(KrpcError::UnknownQueryMethod(
            String::from_utf8_lossy(other).into_owned(),
        )),
    }
}

fn decode_response(r: &BTreeMap<Cow<'_, [u8]>, Value<'_>>) -> Result<Response, KrpcError> {
    let id = node_id_field(r, b"id", "r.id")?;

    let nodes = if let Some(n) = r.get(b"nodes".as_slice()) {
        let bytes = n.as_bytes().ok_or(KrpcError::WrongFieldType {
            field: "r.nodes",
            reason: "not bytes",
        })?;
        if bytes.len() > MAX_NODES_BYTES {
            return Err(KrpcError::Oversize {
                field: "r.nodes",
                actual: bytes.len(),
                max: MAX_NODES_BYTES,
            });
        }
        if !bytes.len().is_multiple_of(COMPACT_NODE_V4_LEN) {
            return Err(KrpcError::MisalignedCompactList {
                which: "nodes",
                len: bytes.len(),
                stride: COMPACT_NODE_V4_LEN,
            });
        }
        bytes
            .chunks_exact(COMPACT_NODE_V4_LEN)
            .filter_map(CompactNode::from_bytes)
            .collect()
    } else {
        Vec::new()
    };

    let nodes6 = if let Some(n) = r.get(b"nodes6".as_slice()) {
        let bytes = n.as_bytes().ok_or(KrpcError::WrongFieldType {
            field: "r.nodes6",
            reason: "not bytes",
        })?;
        if bytes.len() > MAX_NODES_BYTES {
            return Err(KrpcError::Oversize {
                field: "r.nodes6",
                actual: bytes.len(),
                max: MAX_NODES_BYTES,
            });
        }
        if !bytes.len().is_multiple_of(COMPACT_NODE_V6_LEN) {
            return Err(KrpcError::MisalignedCompactList {
                which: "nodes6",
                len: bytes.len(),
                stride: COMPACT_NODE_V6_LEN,
            });
        }
        bytes
            .chunks_exact(COMPACT_NODE_V6_LEN)
            .filter_map(CompactNode6::from_bytes)
            .collect()
    } else {
        Vec::new()
    };

    let values = if let Some(v) = r.get(b"values".as_slice()) {
        let list = v.as_list().ok_or(KrpcError::WrongFieldType {
            field: "r.values",
            reason: "not a list",
        })?;
        if list.len() > MAX_VALUES_PEERS {
            return Err(KrpcError::Oversize {
                field: "r.values",
                actual: list.len(),
                max: MAX_VALUES_PEERS,
            });
        }
        let mut peers = Vec::with_capacity(list.len());
        for entry in list {
            let bytes = entry.as_bytes().ok_or(KrpcError::WrongFieldType {
                field: "r.values[i]",
                reason: "not bytes",
            })?;
            peers.push(decode_compact_addr(bytes, "r.values[i]")?);
        }
        peers
    } else {
        Vec::new()
    };

    let token = if let Some(t) = r.get(b"token".as_slice()) {
        let bytes = t.as_bytes().ok_or(KrpcError::WrongFieldType {
            field: "r.token",
            reason: "not bytes",
        })?;
        if bytes.len() > MAX_TOKEN_LEN {
            return Err(KrpcError::Oversize {
                field: "r.token",
                actual: bytes.len(),
                max: MAX_TOKEN_LEN,
            });
        }
        Some(bytes.to_vec())
    } else {
        None
    };

    Ok(Response {
        id,
        nodes,
        nodes6,
        values,
        token,
    })
}

fn decode_error_payload(v: &Value<'_>) -> Result<KrpcErrorPayload, KrpcError> {
    let list = v.as_list().ok_or(KrpcError::WrongFieldType {
        field: "e",
        reason: "not a list",
    })?;
    if list.len() < 2 {
        return Err(KrpcError::WrongFieldType {
            field: "e",
            reason: "expected [code, message]",
        });
    }
    let code_i64 = list[0].as_int().ok_or(KrpcError::WrongFieldType {
        field: "e[0]",
        reason: "not an integer",
    })?;
    let code = i32::try_from(code_i64).map_err(|_| KrpcError::OutOfRange {
        field: "e[0]",
        value: code_i64,
    })?;
    let message_bytes = list[1].as_bytes().ok_or(KrpcError::WrongFieldType {
        field: "e[1]",
        reason: "not bytes",
    })?;
    if message_bytes.len() > MAX_ERROR_MESSAGE_LEN {
        return Err(KrpcError::Oversize {
            field: "e[1]",
            actual: message_bytes.len(),
            max: MAX_ERROR_MESSAGE_LEN,
        });
    }
    Ok(KrpcErrorPayload {
        code,
        message: String::from_utf8_lossy(message_bytes).into_owned(),
    })
}

fn decode_compact_addr(bytes: &[u8], field: &'static str) -> Result<SocketAddr, KrpcError> {
    match bytes.len() {
        COMPACT_PEER_V4_LEN => {
            let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            let port = u16::from_be_bytes([bytes[4], bytes[5]]);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        COMPACT_PEER_V6_LEN => {
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&bytes[..16]);
            let port = u16::from_be_bytes([bytes[16], bytes[17]]);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(ip),
                port,
                0,
                0,
            )))
        }
        _ => Err(KrpcError::InvalidLength {
            field,
            expected: COMPACT_PEER_V4_LEN,
            actual: bytes.len(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

fn bytes_field<'a>(
    dict: &'a BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    key: &[u8],
    field: &'static str,
) -> Result<&'a [u8], KrpcError> {
    dict.get(key)
        .ok_or(KrpcError::MissingField(field))?
        .as_bytes()
        .ok_or(KrpcError::WrongFieldType {
            field,
            reason: "not bytes",
        })
}

fn bytes_owned_field(
    dict: &BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    key: &[u8],
    field: &'static str,
    max: usize,
) -> Result<Vec<u8>, KrpcError> {
    let bytes = bytes_field(dict, key, field)?;
    if bytes.len() > max {
        return Err(KrpcError::Oversize {
            field,
            actual: bytes.len(),
            max,
        });
    }
    Ok(bytes.to_vec())
}

fn int_field(
    dict: &BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    key: &[u8],
    field: &'static str,
) -> Result<i64, KrpcError> {
    dict.get(key)
        .ok_or(KrpcError::MissingField(field))?
        .as_int()
        .ok_or(KrpcError::WrongFieldType {
            field,
            reason: "not an integer",
        })
}

fn node_id_field(
    dict: &BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    key: &[u8],
    field: &'static str,
) -> Result<NodeId, KrpcError> {
    let bytes = bytes_field(dict, key, field)?;
    if bytes.len() != 20 {
        return Err(KrpcError::InvalidLength {
            field,
            expected: 20,
            actual: bytes.len(),
        });
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(bytes);
    Ok(NodeId::from_bytes(out))
}

fn info_hash_field(
    dict: &BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    key: &[u8],
    field: &'static str,
) -> Result<InfoHash, KrpcError> {
    let bytes = bytes_field(dict, key, field)?;
    if bytes.len() != INFO_HASH_LEN {
        return Err(KrpcError::InvalidLength {
            field,
            expected: INFO_HASH_LEN,
            actual: bytes.len(),
        });
    }
    let mut out = [0u8; INFO_HASH_LEN];
    out.copy_from_slice(bytes);
    Ok(InfoHash::from_bytes(out))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_id(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 20])
    }

    fn sample_hash(byte: u8) -> InfoHash {
        InfoHash::from_bytes([byte; INFO_HASH_LEN])
    }

    fn v4(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
    }

    #[test]
    fn ping_roundtrip() {
        let msg = KrpcMessage {
            transaction_id: b"aa".to_vec(),
            kind: KrpcKind::Query(Query::Ping {
                id: sample_id(0x01),
            }),
            client_version: None,
            ip: None,
        };
        let encoded = msg.encode();
        let decoded = KrpcMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn find_node_roundtrip() {
        let msg = KrpcMessage {
            transaction_id: b"xy".to_vec(),
            kind: KrpcKind::Query(Query::FindNode {
                id: sample_id(0x01),
                target: sample_id(0x02),
            }),
            client_version: Some(b"MP01".to_vec()),
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn get_peers_roundtrip() {
        let msg = KrpcMessage {
            transaction_id: b"gp".to_vec(),
            kind: KrpcKind::Query(Query::GetPeers {
                id: sample_id(0x01),
                info_hash: sample_hash(0xab),
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn announce_peer_roundtrip_with_implied_port() {
        let msg = KrpcMessage {
            transaction_id: b"ap".to_vec(),
            kind: KrpcKind::Query(Query::AnnouncePeer {
                id: sample_id(0x01),
                info_hash: sample_hash(0xcd),
                port: 6881,
                implied_port: true,
                token: vec![0xde, 0xad, 0xbe, 0xef],
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn announce_peer_roundtrip_without_implied_port() {
        let msg = KrpcMessage {
            transaction_id: b"ap".to_vec(),
            kind: KrpcKind::Query(Query::AnnouncePeer {
                id: sample_id(0x01),
                info_hash: sample_hash(0xcd),
                port: 51413,
                implied_port: false,
                token: vec![1, 2, 3, 4, 5, 6, 7, 8],
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn response_with_v4_nodes_roundtrip() {
        let nodes = vec![
            CompactNode {
                id: sample_id(0x11),
                addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 6881),
            },
            CompactNode {
                id: sample_id(0x22),
                addr: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 51413),
            },
        ];
        let msg = KrpcMessage {
            transaction_id: b"r1".to_vec(),
            kind: KrpcKind::Response(Response {
                id: sample_id(0x33),
                nodes,
                ..Default::default()
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn response_with_values_and_token_roundtrip() {
        let msg = KrpcMessage {
            transaction_id: b"r2".to_vec(),
            kind: KrpcKind::Response(Response {
                id: sample_id(0x33),
                values: vec![v4([192, 168, 1, 1], 6881), v4([192, 168, 1, 2], 51413)],
                token: Some(b"tok".to_vec()),
                ..Default::default()
            }),
            client_version: Some(b"MP01".to_vec()),
            ip: Some(v4([203, 0, 113, 42], 7000)),
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn error_roundtrip() {
        let msg = KrpcMessage {
            transaction_id: b"ee".to_vec(),
            kind: KrpcKind::Error(KrpcErrorPayload {
                code: KrpcErrorPayload::CODE_METHOD_UNKNOWN,
                message: "method unknown".into(),
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn ipv6_compact_addr_roundtrip() {
        let v6: SocketAddr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 6881, 0, 0));
        let bytes = encode_compact_addr(&v6);
        assert_eq!(bytes.len(), COMPACT_PEER_V6_LEN);
        assert_eq!(decode_compact_addr(&bytes, "test").unwrap(), v6);
    }

    #[test]
    fn compact_node_v4_roundtrip_bytes() {
        let n = CompactNode {
            id: sample_id(0x42),
            addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 1), 0x1234),
        };
        let b = n.to_bytes();
        assert_eq!(b.len(), COMPACT_NODE_V4_LEN);
        assert_eq!(CompactNode::from_bytes(&b), Some(n));
    }

    #[test]
    fn compact_node_v6_roundtrip_bytes() {
        let n = CompactNode6 {
            id: sample_id(0x42),
            addr: SocketAddrV6::new(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 6881, 0, 0),
        };
        let b = n.to_bytes();
        assert_eq!(b.len(), COMPACT_NODE_V6_LEN);
        assert_eq!(CompactNode6::from_bytes(&b), Some(n));
    }

    // -----------------------------------------------------------------
    // Negative / hostile input
    // -----------------------------------------------------------------

    #[test]
    fn decode_rejects_non_bencode() {
        let err = KrpcMessage::decode(b"not bencode").unwrap_err();
        assert!(matches!(err, KrpcError::Decode(_)));
    }

    #[test]
    fn decode_rejects_non_dict_root() {
        // `i42e` is a valid bencode integer, not a dict.
        let err = KrpcMessage::decode(b"i42e").unwrap_err();
        assert!(matches!(err, KrpcError::Decode(_)));
    }

    #[test]
    fn decode_missing_transaction_id_errors() {
        // {y: q, q: ping, a: {id: 20b}}
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
        dict.insert(Cow::Borrowed(b"q"), Value::Bytes(Cow::Borrowed(b"ping")));
        let mut a: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        a.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        dict.insert(Cow::Borrowed(b"a"), Value::Dict(a));
        let encoded = encode(&Value::Dict(dict));

        let err = KrpcMessage::decode(&encoded).unwrap_err();
        assert!(matches!(err, KrpcError::MissingField("t")));
    }

    #[test]
    fn decode_rejects_oversized_transaction_id() {
        let msg = KrpcMessage {
            transaction_id: vec![0x00; MAX_TRANSACTION_ID_LEN + 1],
            kind: KrpcKind::Query(Query::Ping {
                id: sample_id(0x01),
            }),
            client_version: None,
            ip: None,
        };
        let err = KrpcMessage::decode(&msg.encode()).unwrap_err();
        assert!(matches!(err, KrpcError::Oversize { field: "t", .. }));
    }

    #[test]
    fn decode_rejects_wrong_length_node_id() {
        // Build a query with `a.id` of only 10 bytes.
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
        dict.insert(Cow::Borrowed(b"q"), Value::Bytes(Cow::Borrowed(b"ping")));
        let mut a: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        a.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 10])),
        );
        dict.insert(Cow::Borrowed(b"a"), Value::Dict(a));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::InvalidLength {
                field: "a.id",
                expected: 20,
                actual: 10,
            }
        ));
    }

    #[test]
    fn decode_rejects_unknown_query_method() {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
        dict.insert(
            Cow::Borrowed(b"q"),
            Value::Bytes(Cow::Borrowed(b"sample_infohashes")),
        );
        let mut a: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        a.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        dict.insert(Cow::Borrowed(b"a"), Value::Dict(a));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(err, KrpcError::UnknownQueryMethod(_)));
    }

    #[test]
    fn decode_rejects_unknown_message_type() {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"x")));
        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(err, KrpcError::UnknownMessageType(_)));
    }

    #[test]
    fn decode_rejects_misaligned_nodes_list() {
        // 27 bytes: not a multiple of 26.
        let mut r: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        r.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        r.insert(
            Cow::Borrowed(b"nodes"),
            Value::Bytes(Cow::Owned(vec![0x00; 27])),
        );
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"r")));
        dict.insert(Cow::Borrowed(b"r"), Value::Dict(r));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::MisalignedCompactList {
                which: "nodes",
                len: 27,
                stride: COMPACT_NODE_V4_LEN,
            }
        ));
    }

    #[test]
    fn decode_rejects_oversized_nodes_payload() {
        let oversize = vec![0x00; MAX_NODES_BYTES + COMPACT_NODE_V4_LEN];
        let mut r: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        r.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        r.insert(Cow::Borrowed(b"nodes"), Value::Bytes(Cow::Owned(oversize)));
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"r")));
        dict.insert(Cow::Borrowed(b"r"), Value::Dict(r));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::Oversize {
                field: "r.nodes",
                ..
            }
        ));
    }

    #[test]
    fn decode_rejects_port_out_of_range() {
        // `announce_peer` with a port of 70000.
        let mut a: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        a.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        a.insert(
            Cow::Borrowed(b"info_hash"),
            Value::Bytes(Cow::Owned(vec![0x02; 20])),
        );
        a.insert(Cow::Borrowed(b"port"), Value::Int(70_000));
        a.insert(
            Cow::Borrowed(b"token"),
            Value::Bytes(Cow::Owned(b"t".to_vec())),
        );
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
        dict.insert(
            Cow::Borrowed(b"q"),
            Value::Bytes(Cow::Borrowed(b"announce_peer")),
        );
        dict.insert(Cow::Borrowed(b"a"), Value::Dict(a));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::OutOfRange {
                field: "a.port",
                value: 70_000,
            }
        ));
    }

    #[test]
    fn decode_rejects_oversized_token() {
        let mut a: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        a.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        a.insert(
            Cow::Borrowed(b"info_hash"),
            Value::Bytes(Cow::Owned(vec![0x02; 20])),
        );
        a.insert(Cow::Borrowed(b"port"), Value::Int(6881));
        a.insert(
            Cow::Borrowed(b"token"),
            Value::Bytes(Cow::Owned(vec![0x00; MAX_TOKEN_LEN + 1])),
        );
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"q")));
        dict.insert(
            Cow::Borrowed(b"q"),
            Value::Bytes(Cow::Borrowed(b"announce_peer")),
        );
        dict.insert(Cow::Borrowed(b"a"), Value::Dict(a));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::Oversize {
                field: "a.token",
                ..
            }
        ));
    }

    #[test]
    fn decode_error_payload_with_known_code() {
        let msg = KrpcMessage {
            transaction_id: b"ee".to_vec(),
            kind: KrpcKind::Error(KrpcErrorPayload {
                code: KrpcErrorPayload::CODE_PROTOCOL,
                message: "malformed packet".into(),
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        let KrpcKind::Error(e) = decoded.kind else {
            panic!("not error")
        };
        assert_eq!(e.code, 203);
        assert_eq!(e.message, "malformed packet");
    }

    #[test]
    fn decode_response_preserves_bep32_both_tables() {
        let v4 = CompactNode {
            id: sample_id(0x11),
            addr: SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 6881),
        };
        let v6 = CompactNode6 {
            id: sample_id(0x22),
            addr: SocketAddrV6::new(Ipv6Addr::LOCALHOST, 51413, 0, 0),
        };
        let msg = KrpcMessage {
            transaction_id: b"rr".to_vec(),
            kind: KrpcKind::Response(Response {
                id: sample_id(0x33),
                nodes: vec![v4],
                nodes6: vec![v6],
                ..Default::default()
            }),
            client_version: None,
            ip: None,
        };
        let decoded = KrpcMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_rejects_values_list_too_long() {
        let list: Vec<Value<'static>> = (0..=MAX_VALUES_PEERS)
            .map(|_| Value::Bytes(Cow::Owned(vec![0, 0, 0, 0, 0, 0])))
            .collect();
        let mut r: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        r.insert(
            Cow::Borrowed(b"id"),
            Value::Bytes(Cow::Owned(vec![0x01; 20])),
        );
        r.insert(Cow::Borrowed(b"values"), Value::List(list));
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(Cow::Borrowed(b"t"), Value::Bytes(Cow::Borrowed(b"tt")));
        dict.insert(Cow::Borrowed(b"y"), Value::Bytes(Cow::Borrowed(b"r")));
        dict.insert(Cow::Borrowed(b"r"), Value::Dict(r));

        let err = KrpcMessage::decode(&encode(&Value::Dict(dict))).unwrap_err();
        assert!(matches!(
            err,
            KrpcError::Oversize {
                field: "r.values",
                ..
            }
        ));
    }
}
