//! Typed errors for tracker interactions.

use thiserror::Error;

/// Error returned when announcing to a tracker fails.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TrackerError {
    /// The tracker URL was malformed or used an unsupported scheme.
    #[error("invalid tracker URL: {0}")]
    InvalidUrl(String),
    /// Network-level failure surfaced by the underlying HTTP client.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The tracker response did not parse as bencode.
    #[error("malformed tracker response: {0}")]
    MalformedResponse(String),
    /// The tracker explicitly returned a `failure reason` field per BEP 3.
    #[error("tracker reported failure: {0}")]
    Failure(String),
    /// The compact peer list was not a multiple of the per-entry size (6 for v4, 18 for v6).
    #[error("malformed compact peer list: length {0} is not a multiple of {1}")]
    CompactPeersTruncated(usize, usize),
}
