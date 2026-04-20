# 0024 — DHT routing table (BEP 5)

- **Status**: proposed
- **Date**: 2026-04-20
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/src/dht/`, BEP 5 §"Routing Table", ADR-0001 (subcrate for DHT)

## Context

BEP 5 DHT (Kademlia) needs a routing table that covers the 160-bit ID
space, handles churn gracefully, and answers `find_node` / `get_peers`
in O(log n). This ADR locks the data structure, bucket sizing, and
the node-quality state machine. Bootstrap policy is in ADR-0025;
token/rate-limit design is in ADR-0026.

Reference: rakshasa/libtorrent's `src/dht/dht_router.cc` + `dht_bucket.cc`
ship a proven 20-year shape we mirror for load-bearing decisions and
depart from where magpie's constraints (async/tokio, `#![forbid(unsafe)]`
in core, general-purpose library target) argue for different choices.

## Decision

### Structure

**Dynamic split-on-demand binary tree of buckets**, not a fixed
160-bucket array. Start with a single bucket covering the entire
`[0, 2^160)` ID space. On insertion into a full bucket whose range
contains the local node ID, split the bucket at its midpoint and
re-home its nodes. Buckets not covering the local ID do not split —
they stay at capacity with the best K nodes we've seen.

Concretely:

```rust
pub struct RoutingTable {
    local_id: NodeId,                      // 20 bytes, BEP 42 salted (ADR-0026)
    buckets: Vec<Bucket>,                  // covers disjoint ID ranges
}

struct Bucket {
    range: RangeInclusive<NodeId>,         // inclusive lower, exclusive upper
    nodes: Vec<Node>,                      // capacity = K
    last_changed: Instant,                 // for staleness refresh
}

pub struct Node {
    id: NodeId,
    addr: SocketAddr,
    quality: NodeQuality,
    last_seen: Instant,
    last_pinged: Option<Instant>,
    consecutive_failures: u8,
}

pub enum NodeQuality { Good, Questionable, Bad }
```

`buckets` is kept sorted by range-lower. Lookups binary-search on the
target ID → O(log n) bucket count = O(1) for realistic swarms (<64
buckets).

### `K` (bucket capacity)

**K = 8**, fixed — matches BEP 5 standard, matches rakshasa
(`dht_bucket.h:19`), matches the `find_node` response cap of 8 compact
nodes (26 bytes each, 208 bytes). Not configurable; departing from
the standard breaks interop.

### Node-quality state machine

Three states, following rakshasa's precedent:

- **Good**: `last_seen < 15 min ago` AND `consecutive_failures == 0`.
- **Questionable**: `last_seen ≥ 15 min ago` OR responded at least once
  previously but is silent now.
- **Bad**: `consecutive_failures ≥ 5`.

Transitions:

- On any valid reply from a node: → `Good`, reset `consecutive_failures`,
  update `last_seen`.
- On timeout of a query we sent: `consecutive_failures += 1`. At 5,
  → `Bad`.
- Background sweep (every 60 s): any node with `last_seen > 15 min` →
  `Questionable`. Ping questionable nodes; if no reply in 30 s,
  `consecutive_failures += 1`.

Eviction priority for a full bucket receiving a new candidate:

1. If the bucket has a `Bad` node → evict the `Bad` node, insert new.
2. Else if the bucket has a `Questionable` node AND the candidate has
   responded recently → ping the questionable node first; evict only
   on ping-timeout.
3. Else reject the new candidate (bucket is full of `Good` nodes).

**Bad nodes are kept in the bucket for 4 hours** before hard removal,
matching rakshasa's `timeout_remove_node`. This gives a stale node a
grace window to come back before we forget it entirely.

### Refresh cadence

- **Per-bucket refresh**: buckets with `last_changed > 15 min ago`
  emit a `find_node(random_id_in_range)` to populate them. Scheduled
  on a 60-s cadence (checks all buckets, fires only those stale).
- **Global sweep**: 60-s cadence. Pings questionable nodes, prunes
  bad nodes after 4 h.

### Invariants

1. Every bucket has a non-empty ID range; ranges are disjoint and cover
   `[0, 2^160)` exactly.
2. The local node's ID is always in the range of exactly one bucket
   (the "local" bucket). Only the local bucket can split.
3. `nodes.len() ≤ K` for every bucket.
4. `consecutive_failures ≤ 5` (once at 5, node is `Bad` and subject
   to eviction).
5. Bucket splitting preserves the ordered-by-range invariant of
   `RoutingTable::buckets`.

## Consequences

Positive:

- O(log n) lookup + O(1) for typical swarm sizes.
- Dynamic splitting means memory scales with the ID space we actually
  care about (nodes near our ID), not a fixed 160-bucket array
  (rakshasa's cleanest decision we're inheriting).
- Explicit node-quality state machine — no ambiguity about when a node
  is evictable.
- Grace window for bad nodes (4 h) handles NAT rebinds / transient
  outages without permanently forgetting a node we've seen.

Negative:

- Split-on-demand is more code than a fixed array. Mitigated by the
  small bucket count (<64 typical) — we don't pay for bucket
  manipulation often.
- 5-failure threshold means a briefly-flaky node can accumulate
  failures before recovering. Acceptable: Questionable → ping →
  success resets the counter.

Neutral:

- No separate "recently active" vs "recently inactive" bit like
  rakshasa's `m_recently_active` / `m_recently_inactive`. Our
  timestamp + failure counter carries the same information.

## Alternatives considered

- **Fixed 160-bucket array** (one per leading-bits prefix). Rejected:
  wastes memory when the swarm is small, and still requires a final
  bucket-local lookup O(log k). Dynamic split-on-demand is strictly
  better.
- **K = 16 or K = 20**. Rejected: interop breaks on `find_node`
  response size (trackers expect 8-compact-node responses). K is
  part of the BEP 5 wire contract.
- **Global LRU across all nodes** instead of per-bucket. Rejected:
  the point of Kademlia is locality — each bucket's 8 slots should
  belong to the closest-K in its range, not to a global competition.
