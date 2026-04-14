//! Single token bucket primitive (ADR-0013).
//!
//! Lock-free on the hot path: `try_consume` is a single `fetch_sub` with
//! underflow rollback. Refill is a single `fetch_add` saturated at capacity,
//! performed only by the refiller task (never from peer tasks).

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// Token bucket. Holds atomic `tokens`, a fixed `capacity`, a target
/// `rate_bps`, and two atomic demand counters consulted by the refiller to
/// decide proportional grants.
///
/// At `rate_bps == u64::MAX` (pass-through), `refill` clamps at capacity but
/// the bucket still ticks — which is the load-bearing plan invariant #3.
///
/// The bucket carries a [`tokio::sync::Notify`] that the refiller signals on
/// every tick (via [`Self::notify_refill`]). Peer tasks park on
/// [`Self::wait_for_refill`] when [`Self::try_consume`] denies, waking
/// exactly when new tokens arrive — avoids the 10 ms polling loop that would
/// otherwise burn CPU or over-sleep against the 100 ms refill cadence.
pub struct TokenBucket {
    tokens: AtomicU64,
    capacity: u64,
    rate_bps: AtomicU64,
    consumed: AtomicU64,
    denied: AtomicU64,
    refill_ticks: AtomicU64,
    refill: Notify,
}

impl std::fmt::Debug for TokenBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBucket")
            .field("tokens", &self.tokens.load(Ordering::Relaxed))
            .field("capacity", &self.capacity)
            .field("rate_bps", &self.rate_bps.load(Ordering::Relaxed))
            .field("consumed", &self.consumed.load(Ordering::Relaxed))
            .field("denied", &self.denied.load(Ordering::Relaxed))
            .field("refill_ticks", &self.refill_ticks.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl TokenBucket {
    /// Fresh bucket starting full.
    #[must_use]
    pub fn new(rate_bps: u64, capacity: u64) -> Self {
        Self {
            tokens: AtomicU64::new(capacity),
            capacity,
            rate_bps: AtomicU64::new(rate_bps),
            consumed: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            refill_ticks: AtomicU64::new(0),
            refill: Notify::new(),
        }
    }

    /// Attempt to consume `bytes`. Returns `true` on success. On failure
    /// (insufficient tokens), increments `denied` by the requested amount
    /// so the refiller sees unmet demand.
    ///
    /// Hot path: one `fetch_sub` + (on miss) one `fetch_add` rollback +
    /// `fetch_add` on `denied`. Two atomics per success, three per miss.
    pub fn try_consume(&self, bytes: u64) -> bool {
        let prev = self.tokens.fetch_sub(bytes, Ordering::AcqRel);
        if prev < bytes {
            // Underflow: roll back.
            self.tokens.fetch_add(bytes, Ordering::AcqRel);
            self.denied.fetch_add(bytes, Ordering::Relaxed);
            return false;
        }
        self.consumed.fetch_add(bytes, Ordering::Relaxed);
        true
    }

    /// Refiller grant. Saturates at `capacity` (unused tokens are lost —
    /// standard token-bucket semantics). Increments the per-bucket refill
    /// tick counter for observability.
    pub fn grant(&self, bytes: u64) {
        self.refill_ticks.fetch_add(1, Ordering::Relaxed);
        if bytes == 0 {
            return;
        }
        // Saturating add + clamp. Load current, compute new = min(cap,
        // current + bytes), CAS in. Only the refiller task calls this, so
        // contention is zero; use Relaxed.
        let mut cur = self.tokens.load(Ordering::Relaxed);
        loop {
            let want = cur.saturating_add(bytes).min(self.capacity);
            if want == cur {
                return;
            }
            match self
                .tokens
                .compare_exchange_weak(cur, want, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Snapshot + reset the demand counters. Called by the refiller once
    /// per tick per bucket.
    pub fn take_demand(&self) -> (u64, u64) {
        let c = self.consumed.swap(0, Ordering::AcqRel);
        let d = self.denied.swap(0, Ordering::AcqRel);
        (c, d)
    }

    /// Configured rate (bytes/sec).
    #[must_use]
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }

    /// Update the rate at runtime.
    pub fn set_rate_bps(&self, rate_bps: u64) {
        self.rate_bps.store(rate_bps, Ordering::Relaxed);
    }

    /// Current token count.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    /// Total refill ticks observed. For test invariant #3 (refiller touches
    /// every tier even at pass-through).
    #[must_use]
    pub fn refill_ticks(&self) -> u64 {
        self.refill_ticks.load(Ordering::Relaxed)
    }

    /// Bucket capacity.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Signal peer tasks waiting on this bucket that a refill tick ran.
    /// Called by the refiller *after* tokens are granted. Uses
    /// `notify_waiters` (not `notify_one`) so every waiter retries — the
    /// refill may have delivered enough for all of them.
    ///
    /// Safe at pass-through: waiters will re-test `try_consume`, succeed
    /// trivially, and proceed.
    pub fn notify_refill(&self) {
        self.refill.notify_waiters();
    }

    /// Park until the refiller signals a refill tick. Peer tasks call this
    /// when [`Self::try_consume`] returns false. Wakes exactly when
    /// [`Self::notify_refill`] fires — zero polling.
    pub async fn wait_for_refill(&self) {
        self.refill.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_bucket_full() {
        let b = TokenBucket::new(1000, 500);
        assert_eq!(b.tokens(), 500);
    }

    #[test]
    fn try_consume_succeeds_within_budget() {
        let b = TokenBucket::new(1000, 500);
        assert!(b.try_consume(100));
        assert_eq!(b.tokens(), 400);
    }

    #[test]
    fn try_consume_fails_over_budget_and_rolls_back() {
        let b = TokenBucket::new(1000, 500);
        assert!(!b.try_consume(600));
        assert_eq!(
            b.tokens(),
            500,
            "failed consume must not leave bucket partially drained"
        );
        let (_c, d) = b.take_demand();
        assert_eq!(d, 600, "denied demand must reflect the requested amount");
    }

    #[test]
    fn demand_accounting_separates_consumed_and_denied() {
        let b = TokenBucket::new(1000, 500);
        assert!(b.try_consume(100));
        assert!(b.try_consume(200));
        assert!(!b.try_consume(500));
        let (c, d) = b.take_demand();
        assert_eq!(c, 300);
        assert_eq!(d, 500);
        // Second take is empty.
        let (c, d) = b.take_demand();
        assert_eq!(c, 0);
        assert_eq!(d, 0);
    }

    #[test]
    fn grant_saturates_at_capacity() {
        let b = TokenBucket::new(1000, 500);
        b.try_consume(300);
        b.grant(1000);
        assert_eq!(b.tokens(), 500);
    }

    #[tokio::test]
    async fn wait_for_refill_wakes_on_notify() {
        // #22 plan — hot-path backpressure. A peer task parked on
        // wait_for_refill must wake exactly when the refiller signals
        // (no polling, no timing). Start with an empty bucket, spawn a
        // waiter, notify, and assert the waiter observes a refill and
        // can consume.
        use std::sync::Arc;
        let b = Arc::new(TokenBucket::new(1000, 500));
        // Drain.
        assert!(b.try_consume(500));
        assert!(!b.try_consume(1));
        let b2 = Arc::clone(&b);
        let waiter = tokio::spawn(async move {
            b2.wait_for_refill().await;
            // After wake, the test's grant should have landed.
            b2.try_consume(100)
        });
        // Give the waiter a beat to park.
        tokio::task::yield_now().await;
        b.grant(250);
        b.notify_refill();
        let got = waiter.await.unwrap();
        assert!(got, "waiter must successfully consume after notify");
    }

    #[test]
    fn passthrough_bucket_still_ticks_refill() {
        // Plan invariant #3: rate = u64::MAX still exercises the refill path.
        let b = TokenBucket::new(u64::MAX, u64::MAX);
        assert_eq!(b.refill_ticks(), 0);
        b.grant(0);
        assert_eq!(b.refill_ticks(), 1, "pass-through bucket must still tick");
    }
}
