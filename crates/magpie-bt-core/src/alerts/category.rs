//! Alert categories and the typed [`Alert`] enum.

use crate::ids::{PeerSlot, TorrentId};

/// A typed event emitted by the magpie engine.
///
/// Variants are deliberately `Copy` so alerts can be passed through the ring
/// without allocation. Heavy payloads live outside the alert — consumers look
/// them up via the engine's query API using the IDs carried here.
///
/// Torrent-scoped variants carry a [`TorrentId`] so multi-torrent consumers
/// can attribute events without polling. Global variants (`StatsTick`,
/// `StatsUpdate`, `Dropped`) carry no torrent context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Alert {
    /// A piece finished downloading and passed hash verification.
    PieceCompleted {
        /// Which torrent this piece belongs to.
        torrent: TorrentId,
        /// Zero-based piece index.
        piece: u32,
    },
    /// A new peer connected.
    PeerConnected {
        /// Which torrent this peer is serving.
        torrent: TorrentId,
        /// Per-connection slot identifier.
        peer: PeerSlot,
    },
    /// A peer disconnected.
    PeerDisconnected {
        /// Which torrent this peer was serving.
        torrent: TorrentId,
        /// Per-connection slot identifier.
        peer: PeerSlot,
    },
    /// Periodic stats tick — consumer queries stats via the engine API.
    StatsTick,
    /// Tracker response received for the given torrent.
    TrackerResponse {
        /// Which torrent the tracker responded for.
        torrent: TorrentId,
    },
    /// Engine-side error event. All current error sources are torrent-scoped.
    Error {
        /// Which torrent encountered the error.
        torrent: TorrentId,
        /// Classification of the error.
        code: AlertErrorCode,
    },
    /// Sentinel reporting that `count` alerts were dropped due to overflow
    /// since the previous drain. Always prepended to the drained batch when
    /// drops occurred.
    Dropped {
        /// Number of alerts lost.
        count: u32,
    },
    /// Torrent transitioned to the fully-complete state (every piece
    /// verified). Fires exactly once per torrent lifecycle per ADR-0019.
    /// Torrents that load complete-from-resume do not emit this — the
    /// alert signals a *transition*, not a state.
    TorrentComplete {
        /// Which torrent completed.
        torrent: TorrentId,
    },
    /// Periodic 1 Hz global stats wake signal (ADR-0014). Consumers query
    /// the engine API for per-torrent counters; this alert is purely a
    /// notification that new data is available.
    StatsUpdate,
}

// Compile-time size guard — keep alerts small for the ring (ADR-0002).
const _: () = assert!(size_of::<Alert>() <= 24);

impl Alert {
    /// Returns the category this alert belongs to.
    #[must_use]
    pub const fn category(&self) -> AlertCategory {
        match self {
            Self::PieceCompleted { .. } | Self::TorrentComplete { .. } => AlertCategory::PIECE,
            Self::PeerConnected { .. } | Self::PeerDisconnected { .. } => AlertCategory::PEER,
            Self::TrackerResponse { .. } => AlertCategory::TRACKER,
            Self::Error { .. } => AlertCategory::ERROR,
            Self::StatsTick | Self::StatsUpdate => AlertCategory::STATS,
            // `Dropped` is infrastructure; always delivered regardless of mask.
            Self::Dropped { .. } => AlertCategory::ALL,
        }
    }
}

/// Classification of an error alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AlertErrorCode {
    /// A storage I/O call failed.
    StorageIo,
    /// A tracker announce failed.
    TrackerFailed,
    /// A peer session produced a protocol error.
    PeerProtocol,
    /// Hash verification of a piece failed.
    HashMismatch,
}

/// Bit-flag set selecting alert categories.
///
/// The engine emits alerts only in the categories present in the queue's
/// current mask; filtered-out alerts are never counted against the queue's
/// capacity and never clone their payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertCategory(pub u32);

impl AlertCategory {
    /// Piece-level events (completion, partial, cancel, etc.).
    pub const PIECE: Self = Self(1 << 0);
    /// Peer lifecycle events.
    pub const PEER: Self = Self(1 << 1);
    /// Tracker interactions.
    pub const TRACKER: Self = Self(1 << 2);
    /// Error events.
    pub const ERROR: Self = Self(1 << 3);
    /// Periodic stats ticks.
    pub const STATS: Self = Self(1 << 4);
    /// All categories (default).
    pub const ALL: Self = Self(u32::MAX);
    /// Empty mask — no alerts delivered.
    pub const NONE: Self = Self(0);

    /// Returns `true` if `self` includes every bit of `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for AlertCategory {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for AlertCategory {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
