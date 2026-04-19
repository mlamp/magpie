//! Session orchestration: per-torrent actor + per-peer task wiring.
//!
//! Architecture (M1 baseline):
//!
//! - [`TorrentSession`] is the per-torrent actor. It owns the
//!   [`Picker`](crate::picker::Picker), the in-progress piece buffers, and
//!   the per-peer registry. It receives [`PeerToSession`] events over an
//!   `mpsc` and dispatches [`SessionToPeer`] commands.
//! - [`PeerConn`] is the per-socket task. It owns the `Framed<S, WireCodec>`
//!   and translates between wire frames and actor-level events.
//!
//! M1 ships a leecher only (no upload-side state; ignores inbound `Request`
//! frames). Disk I/O is currently performed inline; the bounded
//! [`DiskWriter`](crate::storage) lands in Phase 4.
//!
//! See ADRs 0009 (peer state machine + Fast extension) and 0010 (request
//! pipelining + endgame) for the design rationale.

pub mod choker;
pub mod disk;
pub mod messages;
pub mod metadata_exchange;
pub mod peer;
pub mod peer_upload;
pub mod pex;
pub mod read_cache;
pub mod resume;
pub mod shaper;
pub mod stats;
pub mod torrent;
pub mod udp;

pub use disk::{
    DEFAULT_DISK_QUEUE_CAPACITY, DiskCompletion, DiskError, DiskMetrics, DiskOp, DiskWriter,
};
pub use messages::{DisconnectReason, PeerSlot, PeerToSession, SessionCommand, SessionToPeer};
pub use metadata_exchange::{MetadataAssembler, MetadataAssemblyError};
pub use resume::{FileResumeSink, ResumeSink, ResumeSinkError, ResumeSnapshot, SCHEMA_VERSION};
pub use peer::{
    DEFAULT_EXTENSION_HANDSHAKE_TIMEOUT, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_PER_PEER_IN_FLIGHT,
    HandshakeError, HandshakeRole, PEER_TO_SESSION_CAPACITY, PeerConfig, PeerConn,
    perform_handshake, read_handshake, write_handshake,
};
pub use torrent::{
    SESSION_COMMAND_CAPACITY, TorrentParams, TorrentParamsError, TorrentSession, TorrentState,
};
