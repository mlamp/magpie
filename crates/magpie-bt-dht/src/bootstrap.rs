//! Bootstrap controller for cold-start DHT join.
//!
//! Drives the three-source contact list (persistent cache → DNS-
//! resolved hosts → consumer-supplied peers, in priority order) +
//! the 60-s ping round loop + the ≥ 32-good-node exit criterion
//! from ADR-0025. DNS resolution itself is out of scope — the
//! caller pre-resolves the hostnames to `SocketAddr`s and hands
//! them in via [`BootstrapConfig::seed_contacts`].
//!
//! # Outputs
//!
//! [`run_bootstrap`] is a one-shot async function that resolves
//! to [`BootstrapOutcome::Operational`] on success,
//! [`BootstrapOutcome::Stalled`] on the 10-min `< 4 good nodes`
//! failure mode, or [`BootstrapOutcome::Cancelled`] if the
//! `DhtRuntime`'s outbound channel closes mid-bootstrap.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures_util::future::join_all;

use crate::krpc::{Query, Response};
use crate::node_id::NodeId;
use crate::runtime::DhtRuntime;

// ---------------------------------------------------------------------------
// Defaults (ADR-0025)
// ---------------------------------------------------------------------------

/// Ping round cadence while bootstrapping.
pub const DEFAULT_ROUND_INTERVAL: Duration = Duration::from_mins(1);

/// Slower cadence post-stall (reduces wake-up load after 10 min).
pub const DEFAULT_STALLED_INTERVAL: Duration = Duration::from_mins(5);

/// Contacts pinged per round.
pub const DEFAULT_PING_BATCH: usize = 8;

/// Exit threshold — good-node count that flips us to `Operational`.
pub const DEFAULT_EXIT_GOOD_NODES: usize = 32;

/// Stall window. If good-node count < [`DEFAULT_STALL_THRESHOLD`]
/// after this long, fire the stall signal.
pub const DEFAULT_STALL_AFTER: Duration = Duration::from_mins(10);

/// Minimum good-node count below which the stall window applies.
pub const DEFAULT_STALL_THRESHOLD: usize = 4;

/// Per-query timeout during a ping round. Short enough that a
/// hostile or dead contact doesn't hold the batch up long.
pub const DEFAULT_CONTACT_QUERY_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Runtime knobs for [`run_bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Seed contact list, highest-priority first. Per ADR-0025,
    /// callers typically build this as: cache entries, then
    /// DNS-resolved bootstrap hostnames, then consumer-supplied
    /// peers (magnet `x.pe`, tracker echoes, …). Deduplicated on
    /// ingest.
    pub seed_contacts: Vec<SocketAddr>,
    /// Ping-round cadence (default [`DEFAULT_ROUND_INTERVAL`]).
    pub round_interval: Duration,
    /// Slower cadence after the stall window elapses.
    pub stalled_interval: Duration,
    /// Contacts pinged per round.
    pub ping_batch: usize,
    /// Good-node count required to flip to `Operational`.
    pub exit_good_nodes: usize,
    /// Window past which `< stall_threshold` good nodes fires a
    /// stall signal.
    pub stall_after: Duration,
    /// Minimum good nodes below which the stall window applies.
    pub stall_threshold: usize,
    /// Per-query deadline during ping rounds.
    pub contact_query_timeout: Duration,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            seed_contacts: Vec::new(),
            round_interval: DEFAULT_ROUND_INTERVAL,
            stalled_interval: DEFAULT_STALLED_INTERVAL,
            ping_batch: DEFAULT_PING_BATCH,
            exit_good_nodes: DEFAULT_EXIT_GOOD_NODES,
            stall_after: DEFAULT_STALL_AFTER,
            stall_threshold: DEFAULT_STALL_THRESHOLD,
            contact_query_timeout: DEFAULT_CONTACT_QUERY_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Terminal state of a [`run_bootstrap`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// Exit criterion met: ≥ `exit_good_nodes` good nodes AND at
    /// least one non-empty `find_node` reply observed.
    Operational,
    /// Stall window elapsed with fewer than `stall_threshold` good
    /// nodes. Callers can continue polling [`DhtRuntime::good_node_count`]
    /// — `run_bootstrap` has exited to free its task.
    Stalled,
    /// The runtime's outbound channel closed before we could reach
    /// either terminal condition.
    Cancelled,
}

// ---------------------------------------------------------------------------
// Bootstrap loop
// ---------------------------------------------------------------------------

/// Drive the bootstrap state machine against `runtime` until one of
/// the [`BootstrapOutcome`] conditions holds.
///
/// Returns [`BootstrapOutcome::Operational`] on success, `Stalled`
/// after the timeout, or `Cancelled` if the outbound transport
/// closes mid-bootstrap.
pub async fn run_bootstrap(runtime: &DhtRuntime, config: BootstrapConfig) -> BootstrapOutcome {
    let started_at = Instant::now();
    let mut pending: VecDeque<SocketAddr> = VecDeque::new();
    let mut seen_non_empty_reply = false;

    // De-duplicate seed contacts in-order.
    for addr in &config.seed_contacts {
        if !pending.contains(addr) {
            pending.push_back(*addr);
        }
    }

    let local_id = runtime.local_id().await;

    loop {
        let good = runtime.good_node_count().await;
        let ever_nonempty = seen_non_empty_reply;
        if good >= config.exit_good_nodes && ever_nonempty {
            return BootstrapOutcome::Operational;
        }

        let stalled = started_at.elapsed() >= config.stall_after && good < config.stall_threshold;
        if stalled {
            return BootstrapOutcome::Stalled;
        }

        let round = pick_round(&mut pending, config.ping_batch);
        if !round.is_empty() {
            let (replies, round_nonempty) =
                run_find_node_round(runtime, local_id, &round, config.contact_query_timeout).await;
            seen_non_empty_reply |= round_nonempty;

            // Fold every discovered (id, addr) into the routing table
            // and enqueue addresses we haven't pinged yet.
            let now = Instant::now();
            for (addr, response) in replies {
                if response.id != local_id {
                    runtime.seed_contact(response.id, addr, now).await;
                }
                for node in &response.nodes {
                    // Kademlia hosts frequently reply with the
                    // querier's own id (we just inserted it on their
                    // side). Skip self-references — inserting them
                    // under a bogus addr pollutes our table.
                    if node.id == local_id {
                        continue;
                    }
                    let node_addr = SocketAddr::V4(node.addr);
                    runtime.seed_contact(node.id, node_addr, now).await;
                    if !pending.contains(&node_addr)
                        && round.iter().all(|existing| *existing != node_addr)
                    {
                        pending.push_back(node_addr);
                    }
                }
            }
        }

        // If the pending list is empty and we still haven't met the
        // exit criterion, keep looping at the stalled cadence so the
        // caller eventually gets `Stalled` rather than a deadlock.
        let interval = if started_at.elapsed() >= config.stall_after {
            config.stalled_interval
        } else {
            config.round_interval
        };
        tokio::time::sleep(interval).await;
    }
}

/// Pick the next `n` pending contacts for the current round.
fn pick_round(pending: &mut VecDeque<SocketAddr>, n: usize) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        if let Some(addr) = pending.pop_front() {
            out.push(addr);
        } else {
            break;
        }
    }
    out
}

/// Fire a round of `find_node(local_id)` in parallel. Returns the
/// `(addr, response)` pairs that replied and whether any reply
/// carried a non-empty `r::nodes` list (one of the exit-criterion
/// components from ADR-0025).
async fn run_find_node_round(
    runtime: &DhtRuntime,
    local_id: NodeId,
    addrs: &[SocketAddr],
    timeout: Duration,
) -> (Vec<(SocketAddr, Response)>, bool) {
    let futs = addrs.iter().map(|addr| {
        let runtime = runtime.clone();
        let target = *addr;
        async move {
            let query = Query::FindNode {
                id: local_id,
                target: local_id,
            };
            tokio::time::timeout(timeout, runtime.send_query(target, query))
                .await
                .ok()
                .and_then(Result::ok)
                .map(|response| (target, response))
        }
    });
    let replies: Vec<(SocketAddr, Response)> = join_all(futs).await.into_iter().flatten().collect();
    let nonempty = replies.iter().any(|(_, r)| !r.nodes.is_empty());
    (replies, nonempty)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_round_drains_up_to_n() {
        let mut q: VecDeque<SocketAddr> = (1..=10u8)
            .map(|i| {
                SocketAddr::V4(std::net::SocketAddrV4::new(
                    std::net::Ipv4Addr::new(10, 0, 0, i),
                    6881,
                ))
            })
            .collect();
        let got = pick_round(&mut q, 4);
        assert_eq!(got.len(), 4);
        assert_eq!(q.len(), 6);
    }

    #[test]
    fn pick_round_empty_queue_returns_empty() {
        let mut q: VecDeque<SocketAddr> = VecDeque::new();
        let got = pick_round(&mut q, 8);
        assert!(got.is_empty());
    }

    #[test]
    fn default_constants_match_adr_0025() {
        assert_eq!(DEFAULT_EXIT_GOOD_NODES, 32);
        assert_eq!(DEFAULT_STALL_THRESHOLD, 4);
        assert_eq!(DEFAULT_STALL_AFTER, Duration::from_secs(10 * 60));
        assert_eq!(DEFAULT_PING_BATCH, 8);
        assert_eq!(DEFAULT_ROUND_INTERVAL, Duration::from_secs(60));
    }
}
