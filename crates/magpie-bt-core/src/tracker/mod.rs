//! HTTP tracker client (BEP 3 announce, BEP 23 compact peer list).
//!
//! The [`Tracker`] trait is the abstraction the engine sees; M1 ships the
//! [`HttpTracker`] implementation. UDP (BEP 15) lands in M2 as a sibling.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

/// Minimum reannounce interval — sessions clamp tracker-returned values up.
///
/// A tracker can technically return `interval: 1` (one second) — within spec,
/// but operationally abusive. Sessions should clamp
/// [`AnnounceResponse::interval`] to at least this floor before scheduling
/// (T2 hardening, follow-up to `parse_announce_response`'s `interval > 0`
/// rejection).
pub const MIN_REANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

mod compact;
mod error;
mod http;
pub mod tiered;
pub mod udp;

pub use error::TrackerError;
pub use http::{
    HttpTracker, build_announce_url, build_scrape_url, parse_response, parse_scrape_response,
};
pub use tiered::TieredTracker;
pub use udp::{MAX_ATTEMPTS as UDP_TRACKER_MAX_ATTEMPTS, UdpTracker};

// Re-export for documentation references.
pub use AnnounceFuture as _AnnounceFutureMarker;

/// Lifecycle event reported in an [`AnnounceRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// First announce after adding the torrent.
    Started,
    /// Final announce after the download completes.
    Completed,
    /// Announce sent on graceful shutdown.
    Stopped,
    /// Routine periodic re-announce (no `event` query parameter).
    Periodic,
}

impl AnnounceEvent {
    /// Wire-format value per BEP 3.
    #[must_use]
    pub const fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
            Self::Periodic => None,
        }
    }
}

/// Parameters for a single tracker announce.
#[derive(Debug, Clone, Copy)]
pub struct AnnounceRequest<'a> {
    /// Torrent info-hash (SHA-1 for v1; truncated SHA-256 for v2).
    pub info_hash: [u8; 20],
    /// Local peer id (20 bytes).
    pub peer_id: [u8; 20],
    /// Port the local peer is listening on.
    pub port: u16,
    /// Bytes uploaded since the `Started` event.
    pub uploaded: u64,
    /// Bytes downloaded since the `Started` event.
    pub downloaded: u64,
    /// Bytes left to download.
    pub left: u64,
    /// Lifecycle event for this announce.
    pub event: AnnounceEvent,
    /// Optional `numwant` hint; trackers honour at their discretion.
    pub num_want: Option<u32>,
    /// Whether to request the BEP 23 compact peer list.
    pub compact: bool,
    /// `tracker id` the tracker handed out on a previous response.
    pub tracker_id: Option<&'a [u8]>,
}

/// Decoded tracker response.
#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    /// Re-announce interval requested by the tracker. Rejected at parse time
    /// if non-positive; sessions should additionally clamp via
    /// [`AnnounceResponse::clamped_interval`] before scheduling.
    pub interval: Duration,
    /// Optional minimum re-announce interval (BEP 23 `min interval`).
    pub min_interval: Option<Duration>,
    /// Peer list, decoded from compact form when present.
    pub peers: Vec<SocketAddr>,
    /// Opaque tracker id to echo on the next announce.
    pub tracker_id: Option<Vec<u8>>,
    /// Estimated number of seeders.
    pub complete: Option<u32>,
    /// Estimated number of leechers.
    pub incomplete: Option<u32>,
    /// Optional human-readable warning.
    pub warning: Option<String>,
}

impl AnnounceResponse {
    /// Re-announce interval clamped to [`MIN_REANNOUNCE_INTERVAL`]. Use this
    /// when scheduling — a hostile tracker that returns `interval: 1` is
    /// within spec but should not be honoured literally.
    #[must_use]
    pub fn clamped_interval(&self) -> Duration {
        self.interval.max(MIN_REANNOUNCE_INTERVAL)
    }
}

/// Boxed future returned by [`Tracker::announce`]. Boxed because the trait is
/// dyn-compatible (`Engine` stores `Arc<dyn Tracker>`); RPIT in trait method
/// returns are not yet object-safe.
pub type AnnounceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AnnounceResponse, TrackerError>> + Send + 'a>>;

/// Trait implemented by tracker transports. M1 ships [`HttpTracker`]; UDP
/// (BEP 15) and DHT (BEP 5) lookups land in later milestones behind the same
/// abstraction.
pub trait Tracker: Send + Sync {
    /// Announce to the tracker.
    fn announce<'a>(&'a self, req: AnnounceRequest<'a>) -> AnnounceFuture<'a>;
}

// ---------------------------------------------------------------------------
// BEP 48 scrape
// ---------------------------------------------------------------------------

/// Per-torrent counters returned by a BEP 48 scrape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrapeFile {
    /// Number of seeders currently in the swarm.
    pub complete: u64,
    /// Number of leechers currently in the swarm.
    pub incomplete: u64,
    /// Cumulative completion count since tracker start.
    pub downloaded: u64,
    /// Optional internal name returned by some trackers.
    pub name: Option<String>,
}

/// Decoded scrape response: per-`info_hash` counters, plus an optional
/// `failure reason` BEP 48 trackers can return in lieu of `files`.
#[derive(Debug, Clone, Default)]
pub struct ScrapeResponse {
    /// Per-`info_hash` counters.
    pub files: std::collections::HashMap<[u8; 20], ScrapeFile>,
    /// Human-readable `failure reason` field.
    pub failure_reason: Option<String>,
}

/// Boxed future returned by [`TrackerScrape::scrape`]. See also
/// [`AnnounceFuture`].
pub type ScrapeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ScrapeResponse, TrackerError>> + Send + 'a>>;

/// Optional companion trait for trackers that expose BEP 48 scrape.
///
/// Split from [`Tracker`] so a minimal tracker impl doesn't have to
/// reason about scrape support — e.g. DHT "trackers" (BEP 5) don't have
/// a scrape concept.
pub trait TrackerScrape: Send + Sync {
    /// Scrape counters for one or more `info_hash`es. Trackers commonly
    /// cap the number of hashes per request (typical: ~64); callers
    /// passing very long lists should batch externally.
    fn scrape<'a>(&'a self, info_hashes: &'a [[u8; 20]]) -> ScrapeFuture<'a>;
}
