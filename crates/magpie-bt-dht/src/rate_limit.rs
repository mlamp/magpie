//! Three-tier token-bucket rate limits per ADR-0026.
//!
//! DHT is a UDP service on the open internet; without rate limits a
//! flood trivially exhausts CPU and memory. The three tiers are:
//!
//! 1. **Per-source-IP inbound** — defence against a single noisy
//!    peer. Sustained 20 qps, burst 60.
//! 2. **Global inbound** — catch-all for a flood via many IPs each
//!    under their per-IP cap. Sustained 500 qps, burst 1500.
//! 3. **Per-remote-node outbound** — stops a runaway lookup from
//!    pounding a single honest node. Sustained 10 qps.
//!
//! Over-cap inbound: **silent drop + counter**. No reply. A rate-
//! limit ack would itself be a reflection amplifier, so we deny
//! ourselves that observability.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Default constants (ADR-0026)
// ---------------------------------------------------------------------------

/// Per-source-IP sustained qps.
pub const DEFAULT_INBOUND_PER_IP_QPS: u32 = 20;
/// Per-source-IP burst cap.
pub const DEFAULT_INBOUND_PER_IP_BURST: u32 = 60;
/// Global inbound sustained qps.
pub const DEFAULT_INBOUND_GLOBAL_QPS: u32 = 500;
/// Global inbound burst cap.
pub const DEFAULT_INBOUND_GLOBAL_BURST: u32 = 1500;
/// Per-remote-node outbound sustained qps.
pub const DEFAULT_OUTBOUND_PER_NODE_QPS: u32 = 10;
/// Per-remote-node outbound burst cap.
pub const DEFAULT_OUTBOUND_PER_NODE_BURST: u32 = 30;

/// Default idle-bucket sweep window. Buckets full of tokens that
/// haven't been touched for this long are pruned.
pub const DEFAULT_BUCKET_IDLE: Duration = Duration::from_mins(5);

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Runtime knobs for [`RateLimiter`]. All fields default to the
/// ADR-0026 constants.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Per-source-IP sustained qps.
    pub inbound_per_ip_qps: u32,
    /// Per-source-IP burst.
    pub inbound_per_ip_burst: u32,
    /// Global inbound qps.
    pub inbound_global_qps: u32,
    /// Global inbound burst.
    pub inbound_global_burst: u32,
    /// Per-remote-node outbound qps.
    pub outbound_per_node_qps: u32,
    /// Per-remote-node outbound burst.
    pub outbound_per_node_burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            inbound_per_ip_qps: DEFAULT_INBOUND_PER_IP_QPS,
            inbound_per_ip_burst: DEFAULT_INBOUND_PER_IP_BURST,
            inbound_global_qps: DEFAULT_INBOUND_GLOBAL_QPS,
            inbound_global_burst: DEFAULT_INBOUND_GLOBAL_BURST,
            outbound_per_node_qps: DEFAULT_OUTBOUND_PER_NODE_QPS,
            outbound_per_node_burst: DEFAULT_OUTBOUND_PER_NODE_BURST,
        }
    }
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_per_sec: u32, now: Instant) -> Self {
        let cap = f64::from(capacity);
        Self {
            tokens: cap,
            capacity: cap,
            refill_per_sec: f64::from(refill_per_sec),
            last_refill: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = elapsed
            .mul_add(self.refill_per_sec, self.tokens)
            .min(self.capacity);
        self.last_refill = now;
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Combined inbound + outbound rate-limit state.
///
/// Construct once per DHT instance. The handler task calls
/// [`Self::check_inbound`] before decoding a datagram's query body;
/// the outbound-query path calls [`Self::check_outbound`] before
/// minting a transaction id.
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    global_inbound: TokenBucket,
    per_ip_inbound: HashMap<IpAddr, TokenBucket>,
    per_node_outbound: HashMap<IpAddr, TokenBucket>,
    dropped_inbound: AtomicU64,
    dropped_outbound: AtomicU64,
}

impl RateLimiter {
    /// Construct with the given config and `now` as the initial
    /// refill instant.
    #[must_use]
    pub fn new(config: RateLimitConfig, now: Instant) -> Self {
        let global_inbound =
            TokenBucket::new(config.inbound_global_burst, config.inbound_global_qps, now);
        Self {
            config,
            global_inbound,
            per_ip_inbound: HashMap::new(),
            per_node_outbound: HashMap::new(),
            dropped_inbound: AtomicU64::new(0),
            dropped_outbound: AtomicU64::new(0),
        }
    }

    /// Admission check for an inbound datagram from `ip`. Returns
    /// `true` when the caller should proceed, `false` when the
    /// request must be silently dropped.
    pub fn check_inbound(&mut self, ip: IpAddr, now: Instant) -> bool {
        let per_ip = self.per_ip_inbound.entry(ip).or_insert_with(|| {
            TokenBucket::new(
                self.config.inbound_per_ip_burst,
                self.config.inbound_per_ip_qps,
                now,
            )
        });
        per_ip.refill(now);
        self.global_inbound.refill(now);

        // Atomic two-bucket consume: only decrement when both have
        // slots, so a rate-limited request does not burn the global
        // budget for a drop.
        if per_ip.tokens < 1.0 || self.global_inbound.tokens < 1.0 {
            self.dropped_inbound.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        per_ip.tokens -= 1.0;
        self.global_inbound.tokens -= 1.0;
        true
    }

    /// Admission check for an outbound query to `ip`. Returns
    /// `true` when the caller should proceed, `false` when the
    /// query must be deferred or dropped (caller policy — ADR-0026
    /// recommends a small per-remote defer queue).
    pub fn check_outbound(&mut self, ip: IpAddr, now: Instant) -> bool {
        let bucket = self.per_node_outbound.entry(ip).or_insert_with(|| {
            TokenBucket::new(
                self.config.outbound_per_node_burst,
                self.config.outbound_per_node_qps,
                now,
            )
        });
        bucket.refill(now);
        if bucket.tokens < 1.0 {
            self.dropped_outbound.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }

    /// Drop per-IP and per-node buckets whose `last_refill` is
    /// older than `max_idle` — a remote that has been quiet that
    /// long will reallocate a fresh burst on their next query,
    /// which is the right outcome. Call periodically (e.g. 60-s
    /// cadence) to keep memory bounded on churn-y swarms.
    pub fn sweep_idle(&mut self, now: Instant, max_idle: Duration) {
        let too_old =
            |b: &TokenBucket| -> bool { now.saturating_duration_since(b.last_refill) >= max_idle };
        self.per_ip_inbound.retain(|_, b| !too_old(b));
        self.per_node_outbound.retain(|_, b| !too_old(b));
    }

    /// Counter — total inbound queries silently dropped since
    /// construction. The milestone gate 6 references this.
    #[must_use]
    pub fn dropped_inbound(&self) -> u64 {
        self.dropped_inbound.load(Ordering::Relaxed)
    }

    /// Counter — total outbound queries dropped since construction.
    #[must_use]
    pub fn dropped_outbound(&self) -> u64 {
        self.dropped_outbound.load(Ordering::Relaxed)
    }

    /// Active per-IP bucket count (observability).
    #[must_use]
    pub fn tracked_source_ips(&self) -> usize {
        self.per_ip_inbound.len()
    }

    /// Active per-node bucket count (observability).
    #[must_use]
    pub fn tracked_outbound_nodes(&self) -> usize {
        self.per_node_outbound.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn per_ip_cap_at_burst_under_flood() {
        // Gate 6 scenario: 200 qps from a single source reaches the
        // 60-burst then drops the rest (all at the same instant, so
        // no refill).
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let src = ip(1, 2, 3, 4);
        let admitted = (0..200).filter(|_| rl.check_inbound(src, t0)).count();
        assert_eq!(admitted, 60, "per-IP burst cap not enforced");
        assert_eq!(rl.dropped_inbound(), 140);
    }

    #[test]
    fn global_cap_at_burst_under_multi_ip_flood() {
        // Gate 6 scenario: 2000 qps spread across 200 IPs (10 each,
        // under per-IP cap). Global burst of 1500 admits 1500, drops
        // 500.
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let mut admitted = 0;
        for peer in 0u16..200 {
            let src = ip(
                10,
                u8::try_from(peer >> 8).unwrap(),
                0,
                u8::try_from(peer & 0xff).unwrap(),
            );
            for _ in 0..10 {
                if rl.check_inbound(src, t0) {
                    admitted += 1;
                }
            }
        }
        assert_eq!(admitted, 1500, "global burst cap not enforced");
        assert_eq!(rl.dropped_inbound(), 500);
    }

    #[test]
    fn per_ip_refills_at_qps_over_time() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let src = ip(1, 2, 3, 4);
        // Drain the 60-burst.
        for _ in 0..60 {
            assert!(rl.check_inbound(src, t0));
        }
        assert!(!rl.check_inbound(src, t0));

        // After 1 s, refill added 20 tokens (per-IP qps = 20).
        let later = t0 + Duration::from_secs(1);
        let admitted = (0..30).filter(|_| rl.check_inbound(src, later)).count();
        assert_eq!(admitted, 20);
    }

    #[test]
    fn outbound_per_node_cap_enforced() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let remote = ip(1, 2, 3, 4);
        let burst = DEFAULT_OUTBOUND_PER_NODE_BURST as usize;
        let admitted = (0..burst * 2)
            .filter(|_| rl.check_outbound(remote, t0))
            .count();
        assert_eq!(admitted, burst);
        assert_eq!(rl.dropped_outbound(), u64::try_from(burst).unwrap());
    }

    #[test]
    fn drops_are_silent_and_counted() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let src = ip(1, 2, 3, 4);
        for _ in 0..100 {
            rl.check_inbound(src, t0);
        }
        assert!(rl.dropped_inbound() >= 40);
    }

    #[test]
    fn sweep_removes_buckets_past_idle_window() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        rl.check_inbound(ip(1, 2, 3, 4), t0);
        assert_eq!(rl.tracked_source_ips(), 1);
        rl.sweep_idle(t0 + Duration::from_secs(1_000), Duration::from_secs(500));
        assert_eq!(rl.tracked_source_ips(), 0);
    }

    #[test]
    fn sweep_keeps_recently_active_buckets() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let src = ip(1, 2, 3, 4);
        rl.check_inbound(src, t0);
        // Sweep soon after — last_refill is recent, bucket retained.
        rl.sweep_idle(t0 + Duration::from_mins(1), Duration::from_secs(500));
        assert_eq!(rl.tracked_source_ips(), 1);
    }

    #[test]
    fn rate_limited_request_does_not_burn_global_budget() {
        // Regression guard for the two-bucket atomic consume. Even
        // if per-IP rejects, the global bucket must not be debited
        // for dropped requests — otherwise the reported cap would
        // be below the actual burst.
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(RateLimitConfig::default(), t0);
        let src = ip(1, 2, 3, 4);
        for _ in 0..1_000 {
            rl.check_inbound(src, t0);
        }
        // Remaining global tokens = burst - 60 = 1440. Use a second
        // IP to confirm the rest of the global budget is intact.
        let mut spread = 0;
        for peer in 0u16..200 {
            let src = ip(
                11,
                u8::try_from(peer >> 8).unwrap(),
                0,
                u8::try_from(peer & 0xff).unwrap(),
            );
            for _ in 0..10 {
                if rl.check_inbound(src, t0) {
                    spread += 1;
                }
            }
        }
        // Global budget was 1500; 60 consumed on the flood → 1440 left.
        assert_eq!(spread, 1440);
    }
}
