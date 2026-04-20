//! Pure KRPC handler functions.
//!
//! Each handler maps a [`Query`] to a [`Response`] (or a protocol
//! [`KrpcErrorPayload`]) by consulting the mutable DHT state:
//! [`RoutingTable`], [`PeerStore`], and [`TokenSecrets`]. The
//! handlers are free of any async / I/O — the dispatch loop wraps
//! them (rate-limiting, transport delivery).
//!
//! Every handler folds the sender into the routing table first —
//! BEP 5 guarantees that receiving a valid query is proof of
//! liveness equivalent to a ping response.

use std::net::SocketAddr;
use std::time::Instant;

use crate::bucket::K;
use crate::krpc::{CompactNode, InfoHash, KrpcErrorPayload, Query, Response};
use crate::node_id::NodeId;
use crate::peer_store::PeerStore;
use crate::routing_table::RoutingTable;
use crate::tokens::TokenSecrets;

/// The mutable state handlers operate on.
///
/// One instance per `Dht`; the dispatch task wraps it in a `Mutex`
/// so handler calls serialise cleanly.
#[derive(Debug)]
pub struct DhtState {
    /// Our own id.
    pub local_id: NodeId,
    /// Routing-table bookkeeping.
    pub routing: RoutingTable,
    /// `announce_peer`-populated peer registry.
    pub peers: PeerStore,
    /// Token factory for `get_peers` / `announce_peer`.
    pub tokens: TokenSecrets,
}

/// Dispatch `query` to the appropriate handler.
///
/// # Errors
///
/// Returns [`KrpcErrorPayload`] for protocol-level rejections
/// (currently only bad `announce_peer` tokens — code 203).
pub fn handle_query(
    state: &mut DhtState,
    query: &Query,
    from: SocketAddr,
    now: Instant,
) -> Result<Response, KrpcErrorPayload> {
    match query {
        Query::Ping { id } => Ok(handle_ping(state, *id, from, now)),
        Query::FindNode { id, target } => Ok(handle_find_node(state, *id, *target, from, now)),
        Query::GetPeers { id, info_hash } => {
            Ok(handle_get_peers(state, *id, *info_hash, from, now))
        }
        Query::AnnouncePeer {
            id,
            info_hash,
            port,
            implied_port,
            token,
        } => handle_announce_peer(
            state,
            *id,
            *info_hash,
            *port,
            *implied_port,
            token,
            from,
            now,
        ),
    }
}

fn handle_ping(state: &mut DhtState, sender: NodeId, from: SocketAddr, now: Instant) -> Response {
    state.routing.insert(sender, from, now);
    Response {
        id: state.local_id,
        ..Default::default()
    }
}

fn handle_find_node(
    state: &mut DhtState,
    sender: NodeId,
    target: NodeId,
    from: SocketAddr,
    now: Instant,
) -> Response {
    state.routing.insert(sender, from, now);
    Response {
        id: state.local_id,
        nodes: closest_v4_nodes(&state.routing, &target),
        ..Default::default()
    }
}

fn handle_get_peers(
    state: &mut DhtState,
    sender: NodeId,
    info_hash: InfoHash,
    from: SocketAddr,
    now: Instant,
) -> Response {
    state.routing.insert(sender, from, now);

    let token = state.tokens.make_token(from.ip()).to_vec();
    let values = state.peers.peers_for(&info_hash, K);
    if values.is_empty() {
        let target = NodeId::from_bytes(*info_hash.as_bytes());
        Response {
            id: state.local_id,
            nodes: closest_v4_nodes(&state.routing, &target),
            token: Some(token),
            ..Default::default()
        }
    } else {
        Response {
            id: state.local_id,
            values,
            token: Some(token),
            ..Default::default()
        }
    }
}

#[allow(clippy::too_many_arguments)] // each field is a BEP 5 wire parameter
fn handle_announce_peer(
    state: &mut DhtState,
    sender: NodeId,
    info_hash: InfoHash,
    port: u16,
    implied_port: bool,
    token: &[u8],
    from: SocketAddr,
    now: Instant,
) -> Result<Response, KrpcErrorPayload> {
    state.routing.insert(sender, from, now);
    if !state.tokens.validate(token, from.ip()) {
        return Err(KrpcErrorPayload {
            code: KrpcErrorPayload::CODE_PROTOCOL,
            message: "bad token".into(),
        });
    }
    let peer_addr = if implied_port {
        from
    } else {
        SocketAddr::new(from.ip(), port)
    };
    state.peers.announce(info_hash, peer_addr, now);
    Ok(Response {
        id: state.local_id,
        ..Default::default()
    })
}

/// Collect up to `K` compact-v4 nodes from the routing table,
/// closest-to-`target` first. Non-v4 nodes are dropped.
fn closest_v4_nodes(routing: &RoutingTable, target: &NodeId) -> Vec<CompactNode> {
    routing
        .find_closest(target, K)
        .into_iter()
        .filter_map(|node| match node.addr {
            SocketAddr::V4(v4) => Some(CompactNode {
                id: node.id,
                addr: v4,
            }),
            SocketAddr::V6(_) => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};

    fn state_with_local(local_id: NodeId, now: Instant) -> DhtState {
        DhtState {
            local_id,
            routing: RoutingTable::new(local_id, now),
            peers: PeerStore::new(crate::peer_store::PeerStoreConfig::default()),
            tokens: TokenSecrets::new(now).unwrap(),
        }
    }

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    fn id(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 20])
    }

    fn hash(byte: u8) -> InfoHash {
        InfoHash::from_bytes([byte; 20])
    }

    #[test]
    fn ping_response_carries_local_id_and_inserts_sender() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        let resp = handle_ping(&mut state, id(0x42), v4(10, 0, 0, 1, 6881), now);
        assert_eq!(resp.id, id(0x01));
        assert_eq!(state.routing.node_count(), 1);
    }

    #[test]
    fn find_node_response_has_closest_nodes() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        for byte in 1u8..=5 {
            state
                .routing
                .insert(id(byte), v4(10, 0, 0, byte, 6881), now);
        }
        let resp = handle_find_node(&mut state, id(0x42), id(0x02), v4(10, 0, 0, 99, 6881), now);
        assert!(!resp.nodes.is_empty());
        assert_eq!(resp.nodes[0].id, id(0x02));
    }

    #[test]
    fn get_peers_returns_nodes_when_no_local_peers() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        for byte in 1u8..=5 {
            state
                .routing
                .insert(id(byte), v4(10, 0, 0, byte, 6881), now);
        }
        let resp = handle_get_peers(
            &mut state,
            id(0x42),
            hash(0xee),
            v4(10, 0, 0, 99, 6881),
            now,
        );
        assert!(resp.values.is_empty());
        assert!(!resp.nodes.is_empty());
        assert!(resp.token.is_some());
    }

    #[test]
    fn get_peers_returns_values_when_peers_known() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        state.peers.announce(hash(0x10), v4(10, 0, 0, 7, 6881), now);
        let resp = handle_get_peers(
            &mut state,
            id(0x42),
            hash(0x10),
            v4(10, 0, 0, 99, 6881),
            now,
        );
        assert_eq!(resp.values, vec![v4(10, 0, 0, 7, 6881)]);
        assert!(resp.nodes.is_empty());
        assert!(resp.token.is_some());
    }

    #[test]
    fn announce_peer_valid_token_records_peer() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        let from = v4(10, 0, 0, 5, 51413);
        let token = state.tokens.make_token(from.ip()).to_vec();

        let resp = handle_announce_peer(
            &mut state,
            id(0x42),
            hash(0x10),
            51413,
            false,
            &token,
            from,
            now,
        )
        .expect("valid token should be accepted");
        assert_eq!(resp.id, id(0x01));
        assert_eq!(state.peers.peer_count(&hash(0x10)), 1);
    }

    #[test]
    fn announce_peer_implied_port_uses_source() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        let from = v4(10, 0, 0, 5, 44444);
        let token = state.tokens.make_token(from.ip()).to_vec();

        handle_announce_peer(
            &mut state,
            id(0x42),
            hash(0x10),
            6881, // ignored because implied_port = true
            true,
            &token,
            from,
            now,
        )
        .unwrap();
        let stored = state.peers.peers_for(&hash(0x10), 8);
        assert_eq!(stored, vec![from]);
    }

    #[test]
    fn announce_peer_bad_token_rejected() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        let from = v4(10, 0, 0, 5, 51413);
        let bogus = [0u8; 8];

        let err = handle_announce_peer(
            &mut state,
            id(0x42),
            hash(0x10),
            51413,
            false,
            &bogus,
            from,
            now,
        )
        .unwrap_err();
        assert_eq!(err.code, 203);
        assert_eq!(state.peers.peer_count(&hash(0x10)), 0);
    }

    #[test]
    fn announce_peer_token_issued_to_different_ip_rejected() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);
        let issuer_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let token = state.tokens.make_token(issuer_ip).to_vec();
        let attacker = v4(10, 0, 0, 6, 51413);
        let err = handle_announce_peer(
            &mut state,
            id(0x42),
            hash(0x10),
            51413,
            false,
            &token,
            attacker,
            now,
        )
        .unwrap_err();
        assert_eq!(err.code, 203);
    }

    #[test]
    fn handle_query_dispatches_all_four_variants() {
        let now = Instant::now();
        let mut state = state_with_local(id(0x01), now);

        let from = v4(10, 0, 0, 5, 6881);
        let ping = Query::Ping { id: id(0x10) };
        assert!(handle_query(&mut state, &ping, from, now).is_ok());

        let find = Query::FindNode {
            id: id(0x10),
            target: id(0x20),
        };
        assert!(handle_query(&mut state, &find, from, now).is_ok());

        let get = Query::GetPeers {
            id: id(0x10),
            info_hash: hash(0x30),
        };
        assert!(handle_query(&mut state, &get, from, now).is_ok());

        let token = state.tokens.make_token(from.ip()).to_vec();
        let announce = Query::AnnouncePeer {
            id: id(0x10),
            info_hash: hash(0x30),
            port: 6881,
            implied_port: false,
            token,
        };
        assert!(handle_query(&mut state, &announce, from, now).is_ok());
    }
}
