//! Split-on-demand Kademlia routing table (ADR-0024).
//!
//! The table starts as a single bucket covering the entire 160-bit
//! id space and splits that bucket on demand whenever an insertion
//! overflows it *and* the bucket contains our local id. Buckets
//! outside the local subtree never split — they stay at capacity
//! [`K`] holding the best nodes seen so far.
//!
//! [`RoutingTable`] is the sync data structure. The async `Dht`
//! task (workstream B/C tail) owns it under a `Mutex` and drives
//! pings from [`Insertion::PendingPing`] and refresh timers from
//! [`RoutingTable::stale_buckets`]. Per ADR-0024, nodes progress
//! through a `Good → Questionable → Bad` state machine; this
//! module owns the insert-side transitions and the background
//! sweeps.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::bucket::{BAD_REMOVE_AFTER, Bucket, Node, NodeQuality, QUESTIONABLE_AFTER};
use crate::{Distance, NodeId};

/// Per-bucket refresh cadence.
///
/// A bucket whose membership has not changed in this long is emitted
/// by [`RoutingTable::stale_buckets`] so the caller can issue a
/// `find_node(random_id_in_range)` to repopulate it. ADR-0024
/// § "Refresh cadence".
pub const BUCKET_REFRESH_AFTER: Duration = QUESTIONABLE_AFTER;

/// Outcome of [`RoutingTable::insert`].
///
/// Variants convey the *side-effect* of the insert so the caller can
/// react asynchronously: ping a questionable node, log an eviction,
/// or record a newly-tracked peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Insertion {
    /// The node was new and was added into a bucket with room.
    Added,
    /// The node was already present; its bookkeeping was refreshed.
    Updated,
    /// The node replaced a [`NodeQuality::Bad`] entry; the evicted
    /// node id is returned so callers can clear any state keyed on
    /// it (e.g. outstanding transaction ids).
    Evicted(NodeId),
    /// The bucket is full of [`NodeQuality::Good`] +
    /// [`NodeQuality::Questionable`] nodes and the candidate is not
    /// evictable yet. The caller should ping this questionable
    /// id; on timeout it becomes [`NodeQuality::Bad`] and a second
    /// `insert` call will [`Insertion::Evicted`] it. ADR-0024
    /// eviction-priority step 2.
    PendingPing(NodeId),
    /// The bucket is full of good nodes, does not contain our local
    /// id, and therefore cannot split. The candidate was dropped.
    Rejected,
}

// ---------------------------------------------------------------------------
// RoutingTable
// ---------------------------------------------------------------------------

/// The live routing table. Holds [`Bucket`]s in strictly sorted,
/// disjoint, contiguous order that covers `[ZERO, MAX]` exactly.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    local_id: NodeId,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    /// Create an empty routing table anchored at `local_id`.
    #[must_use]
    pub fn new(local_id: NodeId, now: Instant) -> Self {
        let root = Bucket::new(NodeId::ZERO, NodeId::MAX, now);
        Self {
            local_id,
            buckets: vec![root],
        }
    }

    /// Our own node id.
    #[must_use]
    pub const fn local_id(&self) -> NodeId {
        self.local_id
    }

    /// Current bucket count (grows as splits happen).
    #[must_use]
    pub const fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Total node count across every bucket.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    /// Count of nodes currently flagged [`NodeQuality::Good`]. Used
    /// by bootstrap to evaluate the ADR-0025 exit criterion.
    #[must_use]
    pub fn good_node_count(&self) -> usize {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| matches!(n.quality, NodeQuality::Good))
            .count()
    }

    /// Iterator over every known node, in no guaranteed order.
    pub fn iter_nodes(&self) -> impl Iterator<Item = &Node> {
        self.buckets.iter().flat_map(|b| b.nodes.iter())
    }

    /// Bucket containing `id` (exists for every id in the 160-bit
    /// space by the table's invariants).
    fn bucket_idx_for(&self, id: &NodeId) -> usize {
        // partition_point finds the first bucket with range_lo > id;
        // that bucket's predecessor contains id.
        let next = self.buckets.partition_point(|b| &b.range_lo <= id);
        debug_assert!(next > 0, "empty predecessor: table invariant broken");
        next - 1
    }

    /// Insert a node (or register a reply from one). Returns the
    /// bookkeeping outcome. Time arithmetic uses `now`.
    pub fn insert(&mut self, id: NodeId, addr: SocketAddr, now: Instant) -> Insertion {
        // Cap iteration at 160: each split shrinks the local bucket
        // by one bit of range, so 160 splits cover a single-id bucket.
        for _ in 0..=160 {
            let idx = self.bucket_idx_for(&id);
            let bucket = &mut self.buckets[idx];

            if let Some(existing) = bucket.nodes.iter_mut().find(|n| n.id == id) {
                existing.addr = addr;
                existing.on_reply(now);
                bucket.last_changed = now;
                return Insertion::Updated;
            }

            if !bucket.is_full() {
                bucket.nodes.push(Node::new_seen(id, addr, now));
                bucket.last_changed = now;
                return Insertion::Added;
            }

            // Full bucket. Eviction priority per ADR-0024:
            //   1. Bad → evict, insert.
            if let Some(bad_idx) = bucket
                .nodes
                .iter()
                .position(|n| matches!(n.quality, NodeQuality::Bad))
            {
                let evicted = bucket.nodes.swap_remove(bad_idx);
                bucket.nodes.push(Node::new_seen(id, addr, now));
                bucket.last_changed = now;
                return Insertion::Evicted(evicted.id);
            }

            //   2. Local bucket? split and retry. If the bucket is
            //      a degenerate single-id range, `try_split_bucket`
            //      returns false and we fall through to step 3/4.
            if bucket.contains(&self.local_id) && self.try_split_bucket(idx, now) {
                continue;
            }

            //   3. Questionable present? ask caller to ping it first.
            let bucket = &self.buckets[idx];
            if let Some(q) = bucket
                .nodes
                .iter()
                .find(|n| matches!(n.quality, NodeQuality::Questionable))
            {
                return Insertion::PendingPing(q.id);
            }

            //   4. Full of Good, non-local → reject.
            return Insertion::Rejected;
        }

        // Bound exceeded — should be unreachable given the 160-bit id
        // space; treat as a hard rejection rather than panicking so a
        // pathological input can't crash the DHT task.
        Insertion::Rejected
    }

    /// Split the bucket at `idx` at its range midpoint. Returns
    /// `false` if the bucket is already a single-id range and cannot
    /// be split further.
    fn try_split_bucket(&mut self, idx: usize, now: Instant) -> bool {
        let old = &self.buckets[idx];
        if old.range_lo == old.range_hi {
            return false;
        }
        let mid = midpoint(&old.range_lo, &old.range_hi);
        // `mid < hi` because `lo < hi`; therefore `mid + 1` cannot
        // overflow past NodeId::MAX unless the bucket is degenerate
        // (already handled above).
        let Some(upper_lo) = successor(&mid) else {
            return false;
        };
        let old = self.buckets.remove(idx);

        let mut lower = Bucket::new(old.range_lo, mid, now);
        let mut upper = Bucket::new(upper_lo, old.range_hi, now);
        for node in old.nodes {
            if lower.contains(&node.id) {
                lower.nodes.push(node);
            } else {
                upper.nodes.push(node);
            }
        }
        // Insert in sorted order: lower first at idx, upper at idx+1.
        self.buckets.insert(idx, upper);
        self.buckets.insert(idx, lower);
        true
    }

    /// Return up to `n` known nodes ordered by ascending XOR distance
    /// to `target`. Useful for answering `find_node` / `get_peers`.
    #[must_use]
    pub fn find_closest(&self, target: &NodeId, n: usize) -> Vec<&Node> {
        let mut scratch: Vec<(Distance, &Node)> = self
            .iter_nodes()
            .map(|node| (node.id.distance(target), node))
            .collect();
        scratch.sort_by_key(|(d, _)| *d);
        scratch.into_iter().take(n).map(|(_, n)| n).collect()
    }

    /// Record a query timeout against `id` (if known). Returns true
    /// iff the node was found and its timeout counter incremented.
    pub fn on_timeout(&mut self, id: &NodeId) -> bool {
        for bucket in &mut self.buckets {
            if let Some(node) = bucket.nodes.iter_mut().find(|n| n.id == *id) {
                node.on_timeout();
                return true;
            }
        }
        false
    }

    /// Background sweep: mark any [`NodeQuality::Good`] node whose
    /// `last_seen` is stale as [`NodeQuality::Questionable`]. Runs
    /// on the 60-s cadence from the `Dht` task; does not send any
    /// network traffic.
    pub fn sweep_quality(&mut self, now: Instant) {
        for bucket in &mut self.buckets {
            for node in &mut bucket.nodes {
                node.refresh_quality(now);
            }
        }
    }

    /// Remove [`NodeQuality::Bad`] nodes whose `last_seen` is older
    /// than [`BAD_REMOVE_AFTER`] (ADR-0024's 4-hour grace window).
    /// Returns the ids of removed nodes so callers can purge any
    /// side-state (outstanding txids, per-remote rate-limit buckets).
    pub fn prune_bad(&mut self, now: Instant) -> Vec<NodeId> {
        let mut removed = Vec::new();
        for bucket in &mut self.buckets {
            let before = bucket.nodes.len();
            bucket.nodes.retain(|node| {
                let expired = matches!(node.quality, NodeQuality::Bad)
                    && now.saturating_duration_since(node.last_seen) >= BAD_REMOVE_AFTER;
                if expired {
                    removed.push(node.id);
                }
                !expired
            });
            if bucket.nodes.len() != before {
                bucket.last_changed = now;
            }
        }
        removed
    }

    /// Bucket indices whose `last_changed` is older than
    /// [`BUCKET_REFRESH_AFTER`]. The caller fires a
    /// `find_node(random_id_in_range)` per returned idx.
    #[must_use]
    pub fn stale_buckets(&self, now: Instant) -> Vec<usize> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| now.saturating_duration_since(b.last_changed) >= BUCKET_REFRESH_AFTER)
            .map(|(i, _)| i)
            .collect()
    }

    /// Inclusive id range covered by bucket `idx`, if it exists.
    #[must_use]
    pub fn bucket_range(&self, idx: usize) -> Option<(NodeId, NodeId)> {
        self.buckets.get(idx).map(|b| (b.range_lo, b.range_hi))
    }
}

// ---------------------------------------------------------------------------
// Big-integer helpers over 20-byte big-endian unsigned ids
// ---------------------------------------------------------------------------

/// `(lo + hi) / 2` without overflow, via the identity
/// `avg = (lo & hi) + ((lo ^ hi) >> 1)`.
fn midpoint(lo: &NodeId, hi: &NodeId) -> NodeId {
    let l = lo.as_bytes();
    let h = hi.as_bytes();
    let mut and_bytes = [0u8; 20];
    let mut xor_bytes = [0u8; 20];
    for (i, slot) in and_bytes.iter_mut().enumerate() {
        *slot = l[i] & h[i];
    }
    for (i, slot) in xor_bytes.iter_mut().enumerate() {
        *slot = l[i] ^ h[i];
    }
    shr1_in_place(&mut xor_bytes);
    add_in_place(&mut and_bytes, &xor_bytes);
    NodeId::from_bytes(and_bytes)
}

/// Successor `id + 1`; returns `None` on overflow from
/// [`NodeId::MAX`].
fn successor(id: &NodeId) -> Option<NodeId> {
    let mut bytes = *id.as_bytes();
    for i in (0..20).rev() {
        if bytes[i] == 0xff {
            bytes[i] = 0;
        } else {
            bytes[i] += 1;
            return Some(NodeId::from_bytes(bytes));
        }
    }
    None
}

/// In-place big-endian right-shift by 1 bit.
fn shr1_in_place(bytes: &mut [u8; 20]) {
    let mut carry = 0u8;
    for byte in bytes.iter_mut() {
        let next_carry = *byte & 1;
        *byte = (*byte >> 1) | (carry << 7);
        carry = next_carry;
    }
}

/// `a += b` (big-endian big-int addition, wrapping is impossible
/// because `a = lo & hi ≤ hi` and `b = (lo ^ hi) >> 1 ≤ (hi - lo)/2`;
/// their sum is the midpoint, always ≤ hi < 2^160).
fn add_in_place(a: &mut [u8; 20], b: &[u8; 20]) {
    let mut carry = 0u16;
    for i in (0..20).rev() {
        let sum = u16::from(a[i]) + u16::from(b[i]) + carry;
        a[i] = u8::try_from(sum & 0xff).expect("masked to 0xff");
        carry = sum >> 8;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::K;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn id(first_byte: u8, tail: u8) -> NodeId {
        let mut b = [tail; 20];
        b[0] = first_byte;
        NodeId::from_bytes(b)
    }

    // -----------------------------------------------------------------
    // Helpers for big-int arithmetic
    // -----------------------------------------------------------------

    #[test]
    fn midpoint_of_zero_and_max_is_top_bit_off() {
        let mid = midpoint(&NodeId::ZERO, &NodeId::MAX);
        // (0 + 2^160-1) / 2 = 2^159 - 1 == [0x7f, 0xff, ...]
        let mut expected = [0xff; 20];
        expected[0] = 0x7f;
        assert_eq!(mid, NodeId::from_bytes(expected));
    }

    #[test]
    fn successor_of_max_is_none() {
        assert_eq!(successor(&NodeId::MAX), None);
    }

    #[test]
    fn successor_of_zero_is_one() {
        let mut expected = [0u8; 20];
        expected[19] = 1;
        assert_eq!(successor(&NodeId::ZERO), Some(NodeId::from_bytes(expected)));
    }

    #[test]
    fn successor_carries_across_byte_boundary() {
        let mut input = [0u8; 20];
        input[19] = 0xff;
        let mut expected = [0u8; 20];
        expected[18] = 1;
        assert_eq!(
            successor(&NodeId::from_bytes(input)),
            Some(NodeId::from_bytes(expected))
        );
    }

    // -----------------------------------------------------------------
    // RoutingTable basics
    // -----------------------------------------------------------------

    #[test]
    fn new_table_starts_with_single_bucket_covering_space() {
        let t = RoutingTable::new(id(0, 0), Instant::now());
        assert_eq!(t.bucket_count(), 1);
        assert_eq!(t.node_count(), 0);
        assert_eq!(t.bucket_range(0), Some((NodeId::ZERO, NodeId::MAX)));
    }

    #[test]
    fn insert_below_k_does_not_split() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        for i in 1..=K {
            let n_id = id(0x80, u8::try_from(i).unwrap());
            let r = t.insert(n_id, addr(6880 + u16::try_from(i).unwrap()), Instant::now());
            assert_eq!(r, Insertion::Added);
        }
        assert_eq!(t.bucket_count(), 1);
        assert_eq!(t.node_count(), K);
    }

    #[test]
    fn insert_updates_existing_node_without_growth() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        let n_id = id(0x80, 0x01);
        assert_eq!(t.insert(n_id, addr(6881), Instant::now()), Insertion::Added);
        assert_eq!(
            t.insert(n_id, addr(6882), Instant::now()),
            Insertion::Updated
        );
        assert_eq!(t.node_count(), 1);
    }

    #[test]
    fn insert_splits_local_bucket_when_full() {
        // Local id high-bit-clear, insertions spread across the id
        // space so a split actually creates two non-empty halves.
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        // Fill the sole bucket to K with ids spanning both halves.
        for i in 0..K {
            let first = if i < 4 { 0x00 } else { 0xc0 };
            let n_id = id(first, u8::try_from(i).unwrap());
            assert!(matches!(
                t.insert(n_id, addr(6881 + u16::try_from(i).unwrap()), Instant::now()),
                Insertion::Added
            ));
        }
        // Now insert a 9th node — must split the (local) bucket.
        let r = t.insert(id(0x10, 0xaa), addr(7000), Instant::now());
        assert!(matches!(r, Insertion::Added));
        assert!(t.bucket_count() >= 2);
    }

    #[test]
    fn insert_rejects_when_full_non_local_good_bucket() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        // Force a split first so we have a non-local sibling.
        for i in 0..K {
            t.insert(
                id(0x00, u8::try_from(i).unwrap()),
                addr(7000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        // These land in the upper (non-local) half.
        for i in 0..K {
            t.insert(
                id(0xc0, u8::try_from(i).unwrap()),
                addr(8000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        // Now try to add one more in the upper half.
        let r = t.insert(id(0xd0, 0xaa), addr(9000), Instant::now());
        assert_eq!(r, Insertion::Rejected);
    }

    #[test]
    fn insert_evicts_bad_node() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        // Fill bucket.
        for i in 0..K {
            t.insert(
                id(0x80, u8::try_from(i).unwrap()),
                addr(7000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        // Drive one node to Bad.
        let bad_id = id(0x80, 0);
        for _ in 0..8 {
            t.on_timeout(&bad_id);
        }
        // Drive the rest to Bad too so the non-local bucket is
        // majority Bad (ensuring the eviction path fires).
        for i in 1..K {
            let nid = id(0x80, u8::try_from(i).unwrap());
            for _ in 0..8 {
                t.on_timeout(&nid);
            }
        }
        // Insert in a *non-local* full bucket — otherwise the local
        // path would split instead of evicting. Build a routing
        // table anchored well away from 0x80.
        // Redo the setup with local=0xff... and force fills in 0x80xx.
        let mut t = RoutingTable::new(id(0xff, 0xff), Instant::now());
        // Force a split so the 0x80 bucket isn't the local one.
        for i in 0..K {
            t.insert(
                id(0x80, u8::try_from(i).unwrap()),
                addr(7000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        for i in 0..K {
            t.insert(
                id(0xc0, u8::try_from(i).unwrap()),
                addr(8000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        // Now drive one 0x80 node to Bad.
        let bad_id = id(0x80, 2);
        for _ in 0..8 {
            t.on_timeout(&bad_id);
        }
        let r = t.insert(id(0x80, 0xee), addr(9000), Instant::now());
        assert_eq!(r, Insertion::Evicted(bad_id));
    }

    #[test]
    fn insert_pending_ping_for_full_bucket_with_questionable() {
        // Build a non-local bucket whose contents are Good +
        // Questionable with no Bad. The insert should return
        // PendingPing targeting the questionable id.
        let t0 = Instant::now();
        let much_later = t0 + QUESTIONABLE_AFTER + Duration::from_secs(1);
        let mut t = RoutingTable::new(id(0xff, 0xff), t0);

        for i in 0..K {
            t.insert(
                id(0x80, u8::try_from(i).unwrap()),
                addr(7000 + u16::try_from(i).unwrap()),
                t0,
            );
        }
        // Force split by filling the local half too.
        for i in 0..K {
            t.insert(
                id(0xc0, u8::try_from(i).unwrap()),
                addr(8000 + u16::try_from(i).unwrap()),
                t0,
            );
        }

        // Age one specific node into Questionable by running the
        // sweep; all 0x80-bucket nodes become Questionable after
        // the deadline.
        t.sweep_quality(much_later);
        let r = t.insert(id(0x80, 0xee), addr(9000), much_later);
        assert!(
            matches!(r, Insertion::PendingPing(_)),
            "expected PendingPing, got {r:?}"
        );
    }

    // -----------------------------------------------------------------
    // find_closest
    // -----------------------------------------------------------------

    #[test]
    fn find_closest_empty_table_returns_empty() {
        let t = RoutingTable::new(NodeId::ZERO, Instant::now());
        assert!(t.find_closest(&NodeId::ZERO, 8).is_empty());
    }

    #[test]
    fn find_closest_returns_xor_ordered_prefix() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        let ids = [
            id(0x00, 0x01),
            id(0x80, 0x00),
            id(0xc0, 0x00),
            id(0xff, 0xff),
            id(0x40, 0x00),
        ];
        for (i, nid) in ids.iter().enumerate() {
            t.insert(*nid, addr(7000 + u16::try_from(i).unwrap()), Instant::now());
        }
        let closest = t.find_closest(&NodeId::ZERO, 3);
        assert_eq!(closest.len(), 3);
        // Closest to ZERO: 0x00,0x01 first, then 0x40,0x00, then 0x80,0x00.
        assert_eq!(closest[0].id, id(0x00, 0x01));
        assert_eq!(closest[1].id, id(0x40, 0x00));
        assert_eq!(closest[2].id, id(0x80, 0x00));
    }

    #[test]
    fn find_closest_caps_at_n() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        for i in 0..5 {
            t.insert(
                id(0x80, u8::try_from(i).unwrap()),
                addr(7000 + u16::try_from(i).unwrap()),
                Instant::now(),
            );
        }
        assert_eq!(t.find_closest(&NodeId::ZERO, 3).len(), 3);
        assert_eq!(t.find_closest(&NodeId::ZERO, 100).len(), 5);
    }

    // -----------------------------------------------------------------
    // Sweeps / stale
    // -----------------------------------------------------------------

    #[test]
    fn sweep_quality_marks_stale_good_questionable() {
        let t0 = Instant::now();
        let mut t = RoutingTable::new(NodeId::ZERO, t0);
        t.insert(id(0x80, 0x01), addr(7000), t0);
        t.sweep_quality(t0 + QUESTIONABLE_AFTER + Duration::from_secs(1));
        let n = t.iter_nodes().next().unwrap();
        assert_eq!(n.quality, NodeQuality::Questionable);
    }

    #[test]
    fn prune_bad_removes_expired_bad_nodes() {
        let t0 = Instant::now();
        let mut t = RoutingTable::new(NodeId::ZERO, t0);
        let nid = id(0x80, 0x01);
        t.insert(nid, addr(7000), t0);
        for _ in 0..8 {
            t.on_timeout(&nid);
        }
        // Within grace window: not pruned.
        assert!(t.prune_bad(t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(t.node_count(), 1);
        // Past grace window: pruned.
        let removed = t.prune_bad(t0 + BAD_REMOVE_AFTER + Duration::from_secs(1));
        assert_eq!(removed, vec![nid]);
        assert_eq!(t.node_count(), 0);
    }

    #[test]
    fn stale_buckets_lists_those_past_refresh_window() {
        let t0 = Instant::now();
        let mut t = RoutingTable::new(NodeId::ZERO, t0);
        t.insert(id(0x80, 1), addr(7000), t0);
        assert!(t.stale_buckets(t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(
            t.stale_buckets(t0 + BUCKET_REFRESH_AFTER + Duration::from_secs(1)),
            vec![0]
        );
    }

    #[test]
    fn on_timeout_on_unknown_id_returns_false() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        assert!(!t.on_timeout(&id(0xff, 0xff)));
    }

    // -----------------------------------------------------------------
    // Invariants
    // -----------------------------------------------------------------

    #[test]
    fn bucket_ranges_stay_disjoint_and_contiguous_after_splits() {
        let mut t = RoutingTable::new(NodeId::ZERO, Instant::now());
        // Drive several splits by filling over K with spread ids.
        for byte in [0x00u8, 0x40, 0x80, 0xc0, 0xe0, 0xf0, 0xf8, 0xfc, 0xfe, 0xff] {
            for j in 0..K {
                t.insert(
                    id(byte, u8::try_from(j).unwrap()),
                    addr(7000 + u16::try_from(j).unwrap()),
                    Instant::now(),
                );
            }
        }
        // Walk buckets and check the union is [ZERO, MAX], ranges
        // disjoint, and capacity holds.
        let mut prev_hi = None;
        for i in 0..t.bucket_count() {
            let (lo, hi) = t.bucket_range(i).unwrap();
            if let Some(p) = prev_hi {
                assert_eq!(successor(&p), Some(lo), "non-contiguous at idx {i}");
            } else {
                assert_eq!(lo, NodeId::ZERO);
            }
            prev_hi = Some(hi);
        }
        assert_eq!(prev_hi, Some(NodeId::MAX));
        for i in 0..t.bucket_count() {
            let b = &t.buckets[i];
            assert!(b.nodes.len() <= K);
        }
    }
}
