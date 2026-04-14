//! Token-bucket refiller task (ADR-0013).
//!
//! Runs every [`REFILL_INTERVAL`]. Visits the session tier first, then the
//! per-torrent tier, then the per-peer tier, granting tokens proportional to
//! demand at each level.
//!
//! Pass-through tiers (rate = `u64::MAX`) still participate — [`run_one_tick`]
//! calls `grant(0)` on them, which bumps the per-bucket refill tick counter
//! without changing token state. This keeps plan invariant #3 (refiller
//! touches every tier, always) mechanically enforceable.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{DuplexBuckets, PeerBuckets, Shaper, TorrentBuckets};

/// Refill cadence. 100 ms per ADR-0013.
pub const REFILL_INTERVAL: Duration = Duration::from_millis(100);

/// Long-running refiller task.
///
/// Spawn once per [`Shaper`]; exit when the shaper is dropped (detected via
/// the Arc weak count test inside the loop — deferred to a future
/// iteration; for M2 just spawn and abort at shutdown).
pub struct Refiller {
    shaper: Arc<Shaper>,
    interval: Duration,
}

impl Refiller {
    /// Construct a refiller for the given shaper.
    #[must_use]
    pub const fn new(shaper: Arc<Shaper>) -> Self {
        Self::with_interval(shaper, REFILL_INTERVAL)
    }

    /// Construct with a custom interval (for tests).
    #[must_use]
    pub const fn with_interval(shaper: Arc<Shaper>, interval: Duration) -> Self {
        Self { shaper, interval }
    }

    /// Run the refill loop until cancelled.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            run_one_tick(&self.shaper, self.interval);
        }
    }
}

/// Execute a single refill pass. Public so tests can drive it synchronously.
///
/// # Panics
///
/// Only if an internal mutex is poisoned.
pub fn run_one_tick(shaper: &Shaper, interval: Duration) {
    shaper.refill_ticks.fetch_add(1, Ordering::Relaxed);

    // Session tier: grant at the configured rate.
    refill_duplex(&shaper.session, interval);

    // Torrent tier: all torrents get a refill pass.
    let torrents = shaper.torrents.lock().expect("shaper torrents poisoned");
    for TorrentBuckets { buckets } in torrents.values() {
        refill_duplex(buckets, interval);
    }
    drop(torrents);

    // Peer tier: all peers get a refill pass.
    let peers = shaper.peers.lock().expect("shaper peers poisoned");
    for PeerBuckets { buckets, .. } in peers.values() {
        refill_duplex(buckets, interval);
    }
    drop(peers);
}

fn refill_duplex(d: &DuplexBuckets, interval: Duration) {
    grant_at_rate(&d.up, interval);
    grant_at_rate(&d.down, interval);
}

fn grant_at_rate(bucket: &super::TokenBucket, interval: Duration) {
    let rate = bucket.rate_bps();
    // Tokens per interval = rate * (interval_ms / 1000). Saturate on
    // overflow (only possible near u64::MAX rates — pass-through). The
    // chain `saturating_mul → /1000 → grant's saturating_add.min(capacity)`
    // is load-bearing for pass-through safety; see #22 plan red-team.
    let ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
    let tokens = rate.saturating_mul(ms) / 1000;
    bucket.grant(tokens);
    // Wake any peer task parked on `wait_for_refill`. Must happen AFTER
    // `grant` so the wakeup sees the tokens it retries against.
    bucket.notify_refill();
    // Demand counters are consulted here so subsequent consume calls start
    // fresh. For M2 we grant at the full configured rate regardless of
    // demand; proportional-to-demand parent→child routing lands alongside
    // M5 cap-enablement per ADR-0013.
    let _ = bucket.take_demand();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::shaper::{DuplexBuckets, Shaper};

    #[tokio::test]
    async fn refill_loop_ticks_at_configured_interval() {
        let shaper = Arc::new(Shaper::new());
        let interval = Duration::from_millis(10);
        let refiller = Refiller::with_interval(Arc::clone(&shaper), interval);
        let handle = tokio::spawn(refiller.run());
        tokio::time::sleep(Duration::from_millis(60)).await;
        handle.abort();
        // Expect roughly 6 ticks ±; be lenient.
        let ticks = shaper.refill_ticks();
        assert!(ticks >= 3, "expected ≥3 ticks, got {ticks}");
    }

    #[test]
    fn run_one_tick_grants_at_session_rate() {
        let mut shaper = Shaper::new();
        // Configure the session up-bucket at 10_000 bytes/sec with 100_000
        // capacity so a 100 ms tick grants 1000 tokens.
        shaper.session = DuplexBuckets::new(10_000, 10_000);
        // Drain the bucket so we can see the grant.
        assert!(shaper.session.up.try_consume(shaper.session.up.capacity()));
        run_one_tick(&shaper, Duration::from_millis(100));
        assert_eq!(shaper.session.up.tokens(), 1000);
    }
}
