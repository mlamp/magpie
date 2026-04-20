//! Iterative Kademlia lookups — the α-parallel recursion that
//! drives `find_node` / `get_peers` / `announce_peer` to completion.
//!
//! The lookup starts with the `K` closest known nodes to the target
//! (from the local routing table + any consumer-seeded contacts),
//! fires `α = 3` parallel queries per round, and continues until
//! either (a) `α` rounds pass with no closer node surfacing, or (b)
//! every known node in the top-`K` closest has been queried. Along
//! the way it collects peers (for `get_peers`) and tokens (for a
//! follow-up `announce_peer`).

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

use futures_util::future::join_all;

use crate::bucket::K;
use crate::krpc::{InfoHash, Query, Response};
use crate::node_id::{Distance, NodeId};
use crate::runtime::DhtRuntime;

/// Kademlia concurrency factor. Three in-flight queries per round is
/// the BEP 5 recommendation and matches rakshasa.
pub const ALPHA: usize = 3;

/// Maximum consecutive "no-new-node" rounds before the lookup is
/// declared converged.
const MAX_STALE_ROUNDS: usize = 3;

/// Hard cap on total rounds regardless of progress (defence against
/// adversarial peers returning a fresh distractor node every round).
pub const MAX_LOOKUP_ROUNDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryState {
    /// Queued; not yet queried.
    Pending,
    /// Query succeeded.
    Responded,
    /// Query failed (timeout, remote error, etc.).
    Failed,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: NodeId,
    addr: SocketAddr,
    state: QueryState,
    token: Option<Vec<u8>>,
}

/// Result of an iterative `get_peers` lookup.
#[derive(Debug, Clone, Default)]
pub struct GetPeersResult {
    /// Peers the lookup collected (may contain duplicates if callers
    /// don't dedup — this field is already deduplicated).
    pub peers: Vec<SocketAddr>,
    /// Closest nodes that returned a token, usable for a follow-up
    /// `announce_peer`. Ordered closest-to-target first.
    pub token_nodes: Vec<(NodeId, SocketAddr, Vec<u8>)>,
}

/// Run an iterative `get_peers(info_hash)` against the DHT.
pub async fn iterative_get_peers(runtime: &DhtRuntime, info_hash: InfoHash) -> GetPeersResult {
    let target = NodeId::from_bytes(*info_hash.as_bytes());
    let local_id = runtime.local_id().await;

    // Seed candidates from our local routing table.
    let mut candidates: BTreeMap<Distance, Candidate> = BTreeMap::new();
    for (id, addr) in runtime.closest_known(&target, K).await {
        candidates.insert(
            id.distance(&target),
            Candidate {
                id,
                addr,
                state: QueryState::Pending,
                token: None,
            },
        );
    }

    let mut peers: HashSet<SocketAddr> = HashSet::new();
    let mut stale_rounds = 0usize;

    for _round in 0..MAX_LOOKUP_ROUNDS {
        let pick: Vec<(Distance, NodeId, SocketAddr)> = candidates
            .iter()
            .filter(|(_, c)| c.state == QueryState::Pending)
            .take(ALPHA)
            .map(|(d, c)| (*d, c.id, c.addr))
            .collect();
        if pick.is_empty() {
            break;
        }

        let futs = pick.into_iter().map(|(d, id, addr)| {
            let runtime = runtime.clone();
            async move {
                let res = runtime
                    .send_query(
                        addr,
                        Query::GetPeers {
                            id: local_id,
                            info_hash,
                        },
                    )
                    .await;
                (d, id, addr, res)
            }
        });
        let results = join_all(futs).await;

        let mut progress = false;
        for (d, _id, addr, res) in results {
            match res {
                Ok(resp) => {
                    if let Some(cand) = candidates.get_mut(&d) {
                        cand.state = QueryState::Responded;
                        cand.token.clone_from(&resp.token);
                    }
                    peers.extend(resp.values.iter().copied());
                    progress |= merge_new_nodes(&mut candidates, &resp, &target);
                    // Make sure the responding node's addr is what we
                    // actually queried (some remotes hairpin via NAT).
                    if let Some(cand) = candidates.get_mut(&d) {
                        cand.addr = addr;
                    }
                }
                Err(_) => {
                    if let Some(cand) = candidates.get_mut(&d) {
                        cand.state = QueryState::Failed;
                    }
                }
            }
        }

        if progress {
            stale_rounds = 0;
        } else {
            stale_rounds += 1;
            if stale_rounds >= MAX_STALE_ROUNDS {
                break;
            }
        }
    }

    let mut token_nodes: Vec<(NodeId, SocketAddr, Vec<u8>)> = candidates
        .values()
        .filter(|c| c.state == QueryState::Responded)
        .filter_map(|c| c.token.clone().map(|t| (c.id, c.addr, t)))
        .collect();
    token_nodes.truncate(K);

    GetPeersResult {
        peers: peers.into_iter().collect(),
        token_nodes,
    }
}

/// Send `announce_peer` to each `(id, addr, token)` in
/// `token_nodes`. Returns the count of acknowledged announces.
pub async fn announce_to_token_nodes(
    runtime: &DhtRuntime,
    info_hash: InfoHash,
    port: u16,
    token_nodes: &[(NodeId, SocketAddr, Vec<u8>)],
) -> usize {
    let local_id = runtime.local_id().await;
    let futs = token_nodes.iter().map(|(_, addr, token)| {
        let runtime = runtime.clone();
        let token = token.clone();
        async move {
            runtime
                .send_query(
                    *addr,
                    Query::AnnouncePeer {
                        id: local_id,
                        info_hash,
                        port,
                        implied_port: false,
                        token,
                    },
                )
                .await
        }
    });
    join_all(futs).await.into_iter().flatten().count()
}

/// Fold `resp.nodes` into the candidate map. Returns `true` iff at
/// least one new id was inserted (≠ "a closer node was found" — any
/// new id counts as forward progress).
fn merge_new_nodes(
    candidates: &mut BTreeMap<Distance, Candidate>,
    resp: &Response,
    target: &NodeId,
) -> bool {
    let mut progress = false;
    for node in &resp.nodes {
        let d = node.id.distance(target);
        if candidates.contains_key(&d) {
            continue;
        }
        // Keep the map bounded to ≤ K entries while the lookup runs:
        // evict the furthest candidate if this newcomer is closer.
        if candidates.len() >= K
            && let Some((&far_d, _)) = candidates.iter().next_back()
        {
            if d < far_d {
                candidates.remove(&far_d);
            } else {
                continue;
            }
        }
        candidates.insert(
            d,
            Candidate {
                id: node.id,
                addr: SocketAddr::V4(node.addr),
                state: QueryState::Pending,
                token: None,
            },
        );
        progress = true;
    }
    progress
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_matches_kademlia_spec() {
        assert_eq!(ALPHA, 3);
    }

    #[test]
    fn get_peers_result_default_is_empty() {
        let r = GetPeersResult::default();
        assert!(r.peers.is_empty());
        assert!(r.token_nodes.is_empty());
    }
}
