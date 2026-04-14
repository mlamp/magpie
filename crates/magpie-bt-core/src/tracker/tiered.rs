//! BEP 12 multi-tracker with tier fall-through and promotion.
//!
//! A torrent's `announce-list` is a list of tiers; each tier is a list of
//! equivalent trackers. On announce, we try trackers within a tier in
//! order. On success, the working tracker is **promoted to the head of
//! its tier** so subsequent announces hit it first (plan invariant #9).
//! On full-tier failure, we fall through to the next tier.
//!
//! Trackers within a tier should be shuffled once on construction so a
//! cold-start doesn't hammer the same tracker from every client — see
//! [`TieredTracker::with_shuffle`] for that path.

use std::sync::Arc;
use std::sync::Mutex;

use super::{AnnounceFuture, AnnounceRequest, Tracker, TrackerError};

/// Multi-tracker wrapper implementing BEP 12 tier fall-through.
pub struct TieredTracker {
    tiers: Mutex<Vec<Vec<Arc<dyn Tracker>>>>,
}

impl TieredTracker {
    /// Build from `announce-list` tiers. Each inner `Vec` is a tier.
    /// Empty inner vecs are filtered out.
    #[must_use]
    pub fn new(tiers: Vec<Vec<Arc<dyn Tracker>>>) -> Self {
        Self {
            tiers: Mutex::new(tiers.into_iter().filter(|t| !t.is_empty()).collect()),
        }
    }

    /// Construct and shuffle each tier. Deterministic seed keeps tests
    /// stable; callers wanting per-session randomness should shuffle before
    /// passing in.
    #[must_use]
    pub fn with_shuffle(mut tiers: Vec<Vec<Arc<dyn Tracker>>>, seed: u64) -> Self {
        let mut counter = seed;
        for tier in &mut tiers {
            // Fisher-Yates with splitmix64.
            for i in (1..tier.len()).rev() {
                counter = counter.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = counter;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                let j = usize::try_from(z).unwrap_or(0) % (i + 1);
                tier.swap(i, j);
            }
        }
        Self::new(tiers)
    }

    /// Snapshot the current tier ordering. For tests + diagnostics.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    #[must_use]
    pub fn tier_order(&self) -> Vec<Vec<*const dyn Tracker>> {
        let tiers = self.tiers.lock().expect("tiers poisoned");
        tiers
            .iter()
            .map(|t| t.iter().map(Arc::as_ptr).collect())
            .collect()
    }

    /// Take a snapshot of the flattened tracker order (tier-major).
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    fn snapshot(&self) -> Vec<(usize, usize, Arc<dyn Tracker>)> {
        let tiers = self.tiers.lock().expect("tiers poisoned");
        let mut out = Vec::new();
        for (ti, tier) in tiers.iter().enumerate() {
            for (ri, t) in tier.iter().enumerate() {
                out.push((ti, ri, Arc::clone(t)));
            }
        }
        drop(tiers);
        out
    }

    /// Move the tracker at `(tier_idx, pos)` to the head of its tier.
    fn promote(&self, tier_idx: usize, pos: usize) {
        let mut tiers = self.tiers.lock().expect("tiers poisoned");
        if let Some(tier) = tiers.get_mut(tier_idx)
            && pos < tier.len()
            && pos > 0
        {
            let item = tier.remove(pos);
            tier.insert(0, item);
        }
    }
}

impl Tracker for TieredTracker {
    fn announce<'a>(&'a self, req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
        let snap = self.snapshot();
        Box::pin(async move {
            let mut last_err: Option<TrackerError> = None;
            for (ti, ri, t) in snap {
                match t.announce(req).await {
                    Ok(resp) => {
                        self.promote(ti, ri);
                        return Ok(resp);
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(last_err.unwrap_or(TrackerError::InvalidUrl("no trackers configured".into())))
        })
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::tracker::{AnnounceEvent, AnnounceResponse};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    struct FailingTracker {
        calls: AtomicU32,
    }
    impl Tracker for FailingTracker {
        fn announce<'a>(&'a self, _req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(TrackerError::Failure("nope".into())) })
        }
    }

    struct OkTracker {
        calls: AtomicU32,
        id: u32,
    }
    impl Tracker for OkTracker {
        fn announce<'a>(&'a self, _req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let id = self.id;
            Box::pin(async move {
                Ok(AnnounceResponse {
                    interval: Duration::from_secs(1800),
                    min_interval: None,
                    peers: Vec::new(),
                    tracker_id: Some(vec![id as u8]),
                    complete: None,
                    incomplete: None,
                    warning: None,
                })
            })
        }
    }

    fn sample_req() -> AnnounceRequest<'static> {
        AnnounceRequest {
            info_hash: [0u8; 20],
            peer_id: [0u8; 20],
            port: 0,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            event: AnnounceEvent::Started,
            num_want: None,
            compact: true,
            tracker_id: None,
        }
    }

    #[tokio::test]
    async fn tier_fall_through_tries_tier_1_when_tier_0_fails() {
        let t1: Arc<dyn Tracker> = Arc::new(FailingTracker {
            calls: AtomicU32::new(0),
        });
        let t2: Arc<dyn Tracker> = Arc::new(OkTracker {
            calls: AtomicU32::new(0),
            id: 7,
        });
        let tiered = TieredTracker::new(vec![vec![Arc::clone(&t1)], vec![Arc::clone(&t2)]]);
        let resp = tiered.announce(sample_req()).await.unwrap();
        assert_eq!(resp.tracker_id, Some(vec![7]));
    }

    #[tokio::test]
    async fn success_promotes_tracker_to_tier_head() {
        let a: Arc<dyn Tracker> = Arc::new(FailingTracker {
            calls: AtomicU32::new(0),
        });
        let b: Arc<dyn Tracker> = Arc::new(OkTracker {
            calls: AtomicU32::new(0),
            id: 9,
        });
        // Within one tier: first fails, second succeeds. On next announce,
        // the second one should be tried first.
        let tiered = TieredTracker::new(vec![vec![Arc::clone(&a), Arc::clone(&b)]]);
        let _ = tiered.announce(sample_req()).await.unwrap();
        // After promotion, b is at position 0, a is at 1.
        let order = tiered.tier_order();
        assert_eq!(order[0][0], Arc::as_ptr(&b));
        assert_eq!(order[0][1], Arc::as_ptr(&a));
    }

    #[tokio::test]
    async fn all_trackers_failing_returns_last_error() {
        let a: Arc<dyn Tracker> = Arc::new(FailingTracker {
            calls: AtomicU32::new(0),
        });
        let b: Arc<dyn Tracker> = Arc::new(FailingTracker {
            calls: AtomicU32::new(0),
        });
        let tiered = TieredTracker::new(vec![vec![a], vec![b]]);
        let err = tiered.announce(sample_req()).await.unwrap_err();
        assert!(matches!(err, TrackerError::Failure(_)));
    }

    #[test]
    fn empty_tiers_filtered() {
        let t: Arc<dyn Tracker> = Arc::new(OkTracker {
            calls: AtomicU32::new(0),
            id: 1,
        });
        let tiered = TieredTracker::new(vec![vec![], vec![t], vec![]]);
        assert_eq!(tiered.tier_order().len(), 1);
    }
}
