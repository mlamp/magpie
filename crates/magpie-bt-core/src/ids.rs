//! Opaque identifier types shared across modules.
//!
//! Extracted here so both [`crate::alerts`] and [`crate::engine`] can
//! reference them without circular module dependencies.

pub use crate::session::messages::PeerSlot;

/// Engine-issued torrent handle.
///
/// Opaque (the inner `u64` is private) — callers receive `TorrentId`s from
/// [`crate::engine::Engine::add_torrent`] and must not mint their own.
/// The engine reserves ID 0 and starts minting from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TorrentId(u64);

impl TorrentId {
    /// Crate-internal constructor used by the engine to mint IDs.
    #[must_use]
    pub(crate) const fn new(n: u64) -> Self {
        Self(n)
    }

    /// Test-only constructor. Not part of the public API contract — the
    /// engine mints ids from an internal counter in production.
    #[doc(hidden)]
    #[must_use]
    pub const fn __test_new(n: u64) -> Self {
        Self(n)
    }
}
