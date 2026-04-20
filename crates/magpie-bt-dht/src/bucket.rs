//! Routing-table node state carriers.
//!
//! Data types for the Kademlia routing table per ADR-0024. The
//! full routing table (split-on-demand binary tree of buckets) plus
//! eviction / refresh policy is workstream C; this module is the
//! load-bearing data model those live on top of — a [`Node`] with
//! its quality state machine, and a [`Bucket`] capped at [`K`]
//! nodes.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::NodeId;

/// Kademlia bucket capacity.
///
/// K = 8 per BEP 5 and ADR-0024. `find_node` responses ship 8 compact
/// nodes (26 bytes each = 208), so this is a wire invariant, not a
/// tuning knob.
pub const K: usize = 8;

/// Failures before a node tips into [`NodeQuality::Bad`].
///
/// Source: ADR-0024 § "Node-quality state machine".
pub const MAX_CONSECUTIVE_FAILURES: u8 = 5;

/// Time since `last_seen` beyond which a [`NodeQuality::Good`] node
/// becomes [`NodeQuality::Questionable`]. ADR-0024.
pub const QUESTIONABLE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Grace window after reaching [`NodeQuality::Bad`] before the node
/// is evicted outright. Kept so a transient NAT rebind doesn't cost
/// us a known contact permanently. ADR-0024 § "Node-quality".
pub const BAD_REMOVE_AFTER: Duration = Duration::from_secs(4 * 60 * 60);

// ---------------------------------------------------------------------------
// NodeQuality
// ---------------------------------------------------------------------------

/// Routing-table state of a remote peer. Transitions driven by
/// query replies / timeouts (ADR-0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeQuality {
    /// Responded recently and without failure.
    Good,
    /// Either stale (`last_seen` > [`QUESTIONABLE_AFTER`] ago) or
    /// silent after a prior good reply.
    Questionable,
    /// At or above [`MAX_CONSECUTIVE_FAILURES`]; evictable.
    Bad,
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// A routing-table entry: a known remote DHT node with bookkeeping
/// for the quality state machine.
#[derive(Debug, Clone)]
pub struct Node {
    /// Remote 160-bit id.
    pub id: NodeId,
    /// UDP address of the remote.
    pub addr: SocketAddr,
    /// Current quality state.
    pub quality: NodeQuality,
    /// When we last received any valid message from this node.
    pub last_seen: Instant,
    /// When we last sent a ping to this node awaiting a reply.
    pub last_pinged: Option<Instant>,
    /// Consecutive ping/query timeouts. Reset on any reply.
    pub consecutive_failures: u8,
}

impl Node {
    /// Construct a freshly-seen node (quality starts [`NodeQuality::Good`],
    /// failure counter zero).
    #[must_use]
    pub const fn new_seen(id: NodeId, addr: SocketAddr, now: Instant) -> Self {
        Self {
            id,
            addr,
            quality: NodeQuality::Good,
            last_seen: now,
            last_pinged: None,
            consecutive_failures: 0,
        }
    }

    /// Record a valid reply. Clears failure counter and marks
    /// [`NodeQuality::Good`].
    pub const fn on_reply(&mut self, now: Instant) {
        self.quality = NodeQuality::Good;
        self.last_seen = now;
        self.consecutive_failures = 0;
    }

    /// Record a query timeout. Increments the failure counter;
    /// at [`MAX_CONSECUTIVE_FAILURES`] the node tips to
    /// [`NodeQuality::Bad`].
    pub const fn on_timeout(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            self.quality = NodeQuality::Bad;
        }
    }

    /// Re-evaluate `Good → Questionable` based on staleness.
    /// Does not touch [`NodeQuality::Bad`] nodes.
    pub fn refresh_quality(&mut self, now: Instant) {
        if matches!(self.quality, NodeQuality::Good)
            && now.saturating_duration_since(self.last_seen) >= QUESTIONABLE_AFTER
        {
            self.quality = NodeQuality::Questionable;
        }
    }
}

// ---------------------------------------------------------------------------
// Bucket
// ---------------------------------------------------------------------------

/// A Kademlia bucket: up to [`K`] nodes sharing a contiguous id
/// range. `range` is inclusive of both ends. Split logic lives in
/// the forthcoming routing-table module (workstream C).
#[derive(Debug, Clone)]
pub struct Bucket {
    /// Lower bound (inclusive) of the id range this bucket covers.
    pub range_lo: NodeId,
    /// Upper bound (inclusive) of the id range this bucket covers.
    pub range_hi: NodeId,
    /// Known nodes in this bucket, `len() ≤ K`.
    pub nodes: Vec<Node>,
    /// Last time this bucket's membership changed — used by the
    /// periodic refresh sweep.
    pub last_changed: Instant,
}

impl Bucket {
    /// Empty bucket covering `[lo, hi]`.
    #[must_use]
    pub fn new(range_lo: NodeId, range_hi: NodeId, now: Instant) -> Self {
        Self {
            range_lo,
            range_hi,
            nodes: Vec::with_capacity(K),
            last_changed: now,
        }
    }

    /// True if `id` falls in this bucket's range.
    #[must_use]
    pub fn contains(&self, id: &NodeId) -> bool {
        id >= &self.range_lo && id <= &self.range_hi
    }

    /// True when the bucket has reached its capacity of [`K`].
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.nodes.len() >= K
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn loopback_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn sample_id(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 20])
    }

    #[test]
    fn bucket_contains_matches_range() {
        let now = Instant::now();
        let lo = sample_id(0x10);
        let hi = sample_id(0x20);
        let bucket = Bucket::new(lo, hi, now);

        assert!(bucket.contains(&lo));
        assert!(bucket.contains(&hi));
        assert!(bucket.contains(&sample_id(0x18)));
        assert!(!bucket.contains(&sample_id(0x00)));
        assert!(!bucket.contains(&sample_id(0x21)));
    }

    #[test]
    fn bucket_is_full_at_k() {
        let now = Instant::now();
        let mut bucket = Bucket::new(NodeId::ZERO, sample_id(0xff), now);
        for i in 0..K {
            let i_u8 = u8::try_from(i).unwrap();
            bucket.nodes.push(Node::new_seen(
                sample_id(i_u8),
                loopback_addr(6881 + u16::from(i_u8)),
                now,
            ));
        }
        assert!(bucket.is_full());
    }

    #[test]
    fn node_on_reply_resets_failures() {
        let now = Instant::now();
        let mut node = Node::new_seen(sample_id(0x42), loopback_addr(6881), now);
        node.consecutive_failures = 3;
        node.quality = NodeQuality::Questionable;

        node.on_reply(now);

        assert_eq!(node.consecutive_failures, 0);
        assert_eq!(node.quality, NodeQuality::Good);
    }

    #[test]
    fn node_on_timeout_tips_bad_at_threshold() {
        let now = Instant::now();
        let mut node = Node::new_seen(sample_id(0x42), loopback_addr(6881), now);

        for _ in 0..(MAX_CONSECUTIVE_FAILURES - 1) {
            node.on_timeout();
        }
        assert_ne!(node.quality, NodeQuality::Bad);

        node.on_timeout();
        assert_eq!(node.quality, NodeQuality::Bad);
        assert_eq!(node.consecutive_failures, MAX_CONSECUTIVE_FAILURES);
    }

    #[test]
    fn node_on_timeout_saturates() {
        let now = Instant::now();
        let mut node = Node::new_seen(sample_id(0x42), loopback_addr(6881), now);
        for _ in 0..300 {
            node.on_timeout();
        }
        assert_eq!(node.consecutive_failures, u8::MAX);
        assert_eq!(node.quality, NodeQuality::Bad);
    }

    #[test]
    fn refresh_quality_marks_stale_good_questionable() {
        let t0 = Instant::now();
        let mut node = Node::new_seen(sample_id(0x42), loopback_addr(6881), t0);
        let later = t0 + QUESTIONABLE_AFTER + Duration::from_secs(1);
        node.refresh_quality(later);
        assert_eq!(node.quality, NodeQuality::Questionable);
    }

    #[test]
    fn refresh_quality_leaves_bad_alone() {
        let t0 = Instant::now();
        let mut node = Node::new_seen(sample_id(0x42), loopback_addr(6881), t0);
        node.quality = NodeQuality::Bad;
        node.refresh_quality(t0 + QUESTIONABLE_AFTER + Duration::from_secs(1));
        assert_eq!(node.quality, NodeQuality::Bad);
    }
}
