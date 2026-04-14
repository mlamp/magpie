#![allow(
    clippy::missing_panics_doc,          // every lock().expect() panics on poison
    clippy::significant_drop_tightening  // guards are intentionally block-scoped
)]
//! Bounded, category-filtered, single-primary-reader alert queue.
//!
//! Implementation is single-buffered with a [`VecDeque`] for O(1) front/back
//! ops. Drain swaps the internal buffer out via [`std::mem::take`], leaving
//! the producer with an empty deque — equivalent in practice to ADR-0002's
//! double-buffering while the primary reader is single-threaded, and simpler
//! to reason about. Overflow policy: the **oldest** alert is evicted to make
//! room for the newest; the number of evictions since the last drain is
//! reported via the [`Alert::Dropped`] sentinel prepended on drain.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::Notify;

use super::category::{Alert, AlertCategory};

/// Bounded alert ring.
///
/// Typical usage: one queue per torrent; the session pushes events from the
/// engine task, and a single consumer drains them in batches.
pub struct AlertQueue {
    inner: Mutex<Inner>,
    notify: Notify,
}

struct Inner {
    buf: VecDeque<Alert>,
    capacity: usize,
    mask: AlertCategory,
    generation: u64,
    dropped_since_swap: u32,
}

impl AlertQueue {
    /// Creates a queue with the given capacity and [`AlertCategory::ALL`]
    /// subscribed.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_mask(capacity, AlertCategory::ALL)
    }

    /// Creates a queue with the given capacity and category mask.
    #[must_use]
    pub fn with_mask(capacity: usize, mask: AlertCategory) -> Self {
        assert!(capacity > 0, "alert queue capacity must be > 0");
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(capacity),
                capacity,
                mask,
                generation: 0,
                dropped_since_swap: 0,
            }),
            notify: Notify::new(),
        }
    }

    /// Updates the category mask. Takes effect for subsequent [`AlertQueue::push`]
    /// calls; alerts already in the buffer are not retroactively filtered.
    pub fn set_mask(&self, mask: AlertCategory) {
        self.inner.lock().expect("poisoned").mask = mask;
    }

    /// Returns the current generation counter. Increments on every drain.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.lock().expect("poisoned").generation
    }

    /// Pushes `alert`. Returns `true` if the alert was accepted into the
    /// buffer, `false` if it was filtered out by the mask.
    ///
    /// Never blocks. If the buffer is full, the oldest entry is evicted and a
    /// drop counter is incremented; the `Alert::Dropped` sentinel will be
    /// prepended to the next drained batch.
    pub fn push(&self, alert: Alert) -> bool {
        let accepted;
        {
            let mut inner = self.inner.lock().expect("poisoned");
            if !inner.mask.contains(alert.category()) && !matches!(alert, Alert::Dropped { .. }) {
                return false;
            }
            if inner.buf.len() == inner.capacity {
                inner.buf.pop_front();
                inner.dropped_since_swap = inner.dropped_since_swap.saturating_add(1);
            }
            inner.buf.push_back(alert);
            accepted = true;
        }
        self.notify.notify_one();
        accepted
    }

    /// Atomically drains the current batch. Returns an empty `Vec` when the
    /// queue is empty. If any alerts were dropped since the previous drain, a
    /// [`Alert::Dropped`] sentinel is prepended.
    #[must_use = "drained alerts are lost if discarded"]
    pub fn drain(&self) -> Vec<Alert> {
        let (dropped, mut taken) = {
            let mut inner = self.inner.lock().expect("poisoned");
            inner.generation = inner.generation.wrapping_add(1);
            let dropped = inner.dropped_since_swap;
            inner.dropped_since_swap = 0;
            let taken: Vec<Alert> = inner.buf.drain(..).collect();
            (dropped, taken)
        };
        if dropped > 0 {
            taken.insert(0, Alert::Dropped { count: dropped });
        }
        taken
    }

    /// Returns once at least one alert is waiting in the queue.
    ///
    /// The internal loop also handles stale-permit wake-ups (a push that
    /// happened before any waiter called `notified()`), so a caller that
    /// observes an empty batch after `wait()` returns may safely call
    /// [`AlertQueue::drain`] unconditionally.
    pub async fn wait(&self) {
        loop {
            if !self.is_empty() {
                return;
            }
            self.notify.notified().await;
            if !self.is_empty() {
                return;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.inner.lock().expect("poisoned").buf.is_empty()
    }

    /// Current number of pending alerts (for tests and observability).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.inner.lock().expect("poisoned").buf.len()
    }
}

impl std::fmt::Debug for AlertQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().expect("poisoned");
        f.debug_struct("AlertQueue")
            .field("capacity", &inner.capacity)
            .field("pending", &inner.buf.len())
            .field("generation", &inner.generation)
            .field("dropped_since_swap", &inner.dropped_since_swap)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::category::{Alert, AlertErrorCode};
    use crate::ids::{PeerSlot, TorrentId};

    const TID: TorrentId = TorrentId::__test_new(1);

    #[test]
    fn push_drain_roundtrip() {
        let q = AlertQueue::new(8);
        q.push(Alert::PieceCompleted { torrent: TID, piece: 1 });
        q.push(Alert::PieceCompleted { torrent: TID, piece: 2 });
        let batch = q.drain();
        assert_eq!(batch.len(), 2);
        assert_eq!(q.drain().len(), 0);
    }

    #[test]
    fn category_mask_filters_push() {
        let q = AlertQueue::with_mask(8, AlertCategory::PIECE);
        assert!(q.push(Alert::PieceCompleted { torrent: TID, piece: 1 }));
        assert!(!q.push(Alert::PeerConnected { torrent: TID, peer: PeerSlot(42) }));
        assert!(!q.push(Alert::StatsTick));
        let batch = q.drain();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn overflow_drops_oldest_and_emits_sentinel() {
        let q = AlertQueue::new(3);
        for i in 0..5 {
            q.push(Alert::PieceCompleted { torrent: TID, piece: i });
        }
        let batch = q.drain();
        // Sentinel first, then 3 most-recent pieces (2, 3, 4).
        assert_eq!(batch.len(), 4);
        assert_eq!(batch[0], Alert::Dropped { count: 2 });
        assert_eq!(batch[1], Alert::PieceCompleted { torrent: TID, piece: 2 });
        assert_eq!(batch[3], Alert::PieceCompleted { torrent: TID, piece: 4 });
    }

    #[test]
    fn dropped_sentinel_resets_after_drain() {
        let q = AlertQueue::new(2);
        q.push(Alert::PieceCompleted { torrent: TID, piece: 1 });
        q.push(Alert::PieceCompleted { torrent: TID, piece: 2 });
        q.push(Alert::PieceCompleted { torrent: TID, piece: 3 });
        let _ = q.drain();
        q.push(Alert::PieceCompleted { torrent: TID, piece: 4 });
        let batch = q.drain();
        assert_eq!(batch, vec![Alert::PieceCompleted { torrent: TID, piece: 4 }]);
    }

    #[test]
    fn generation_increments_on_drain() {
        let q = AlertQueue::new(4);
        let gen0 = q.generation();
        q.push(Alert::StatsTick);
        let _ = q.drain();
        assert_eq!(q.generation(), gen0 + 1);
    }

    #[test]
    fn mask_always_passes_dropped_sentinel() {
        // Even a NONE mask should let the engine report losses if any get enqueued.
        let q = AlertQueue::new(1);
        q.set_mask(AlertCategory::NONE);
        // Push a Dropped sentinel directly (simulating engine bypass).
        assert!(q.push(Alert::Dropped { count: 99 }));
        let batch = q.drain();
        assert_eq!(batch, vec![Alert::Dropped { count: 99 }]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_wakes_on_push() {
        use std::sync::Arc;
        let q = Arc::new(AlertQueue::new(4));
        let q2 = Arc::clone(&q);
        let handle = tokio::spawn(async move {
            q2.wait().await;
            q2.drain()
        });
        tokio::task::yield_now().await;
        q.push(Alert::Error {
            torrent: TID,
            code: AlertErrorCode::PeerProtocol,
        });
        let batch = handle.await.unwrap();
        assert_eq!(batch.len(), 1);
    }
}
