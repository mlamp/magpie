//! Internal channel types between the per-torrent actor
//! ([`TorrentSession`](super::torrent::TorrentSession)) and per-peer tasks
//! ([`PeerConn`](super::peer::PeerConn)).

use bytes::Bytes;
use tokio::sync::mpsc;

use magpie_bt_wire::BlockRequest;

/// Opaque per-connection identifier issued by the torrent actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerSlot(pub u64);

/// Messages flowing **from a peer task into the torrent actor**.
///
/// Public so that bespoke peer transports (e.g. tests, mock implementations,
/// future uTP) can construct and drive a `TorrentSession`. The variant set is
/// `#[non_exhaustive]`; magpie may add new event kinds without bumping
/// SemVer-major.
#[derive(Debug)]
#[non_exhaustive]
#[allow(missing_docs)] // field-level docs would just restate variant names.
pub enum PeerToSession {
    /// Handshake completed; peer is fully attached.
    Connected {
        slot: PeerSlot,
        peer_id: [u8; 20],
        supports_fast: bool,
    },
    /// Peer choked us.
    Choked { slot: PeerSlot },
    /// Peer unchoked us.
    Unchoked { slot: PeerSlot },
    /// Peer announced a new piece.
    Have { slot: PeerSlot, piece: u32 },
    /// Peer's full bitfield (raw bytes).
    Bitfield { slot: PeerSlot, bytes: Bytes },
    /// BEP 6: peer sent `HaveAll`.
    HaveAll { slot: PeerSlot },
    /// BEP 6: peer sent `HaveNone`.
    HaveNone { slot: PeerSlot },
    /// Block payload received.
    BlockReceived {
        slot: PeerSlot,
        piece: u32,
        offset: u32,
        data: Bytes,
    },
    /// BEP 6 reject of a request we made.
    Rejected { slot: PeerSlot, req: BlockRequest },
    /// Peer is interested in downloading from us.
    Interested { slot: PeerSlot },
    /// Peer is no longer interested in downloading from us.
    NotInterested { slot: PeerSlot },
    /// Peer requested a block from us (M2 upload path).
    BlockRequested { slot: PeerSlot, req: BlockRequest },
    /// Peer cancelled a previously requested block.
    RequestCancelled { slot: PeerSlot, req: BlockRequest },
    /// Peer disconnected (cleanly or with an error).
    Disconnected {
        slot: PeerSlot,
        reason: DisconnectReason,
    },
}

/// Reason a peer task is exiting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// EOF on the socket (peer closed cleanly).
    Eof,
    /// Local shutdown signalled.
    Shutdown,
    /// Wire-protocol or framing error.
    ProtocolError(String),
    /// Underlying I/O error.
    Io(String),
}

/// Out-of-band command to a [`TorrentSession`](super::torrent::TorrentSession),
/// typically issued by the [`Engine`](crate::engine::Engine).
///
/// Distinct from [`PeerToSession`] (which is the steady-state event stream
/// from peer tasks): commands cover lifecycle plumbing — registering a freshly
/// connected peer, requesting graceful shutdown — that the Engine drives.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionCommand {
    /// Attach a new peer task to the session. The peer's `SessionToPeer`
    /// sender comes in here so the actor can route commands back.
    RegisterPeer {
        /// Engine-issued slot id; must be unique within the session.
        slot: PeerSlot,
        /// Sender side of the peer's `SessionToPeer` channel.
        tx: mpsc::UnboundedSender<SessionToPeer>,
        /// Maximum in-flight requests this peer is allowed to hold.
        max_in_flight: u32,
        /// Negotiated fast-ext bit — `true` only when **both** sides
        /// advertised BEP 6 in their handshake. Lets
        /// [`TorrentSession::register_peer_with`] use the 1-byte
        /// `HaveAll`/`HaveNone` shortcut instead of a full `Bitfield`
        /// when the current `have` bitmap is all-1 or all-0 (#21 plan).
        supports_fast: bool,
    },
    /// Pause the torrent: stop scheduling new requests, send `Choke` to all
    /// peers (we keep their connections open so resume is cheap), but the
    /// actor stays alive. G1 per `docs/api-audit.md`. Idempotent — pausing
    /// an already-paused torrent is a no-op.
    Pause,
    /// Resume after [`SessionCommand::Pause`]. Sends `Unchoke` to peers that
    /// were interested at pause time (M2 baseline) and re-enables
    /// scheduling. Idempotent.
    Resume,
    /// Initiate graceful shutdown of the torrent: peers receive
    /// `SessionToPeer::Shutdown`, the actor exits its run loop.
    Shutdown,
}

/// Messages flowing **from the torrent actor to a peer task**.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionToPeer {
    /// Set our `interested` state on this peer.
    SetInterested(bool),
    /// Issue a block request.
    Request(BlockRequest),
    /// Cancel a previously issued request.
    Cancel(BlockRequest),
    /// Tell the peer we now have this piece (we just verified it).
    Have(u32),
    /// Send the initial Bitfield advertisement after handshake. Bytes are
    /// already wire-encoded (high-bit first, spare bits zero per BEP 3).
    /// Sent by the actor in `handle_connected` so the peer learns what we
    /// have and can decide whether to be interested.
    SendBitfield(Vec<u8>),
    /// BEP 6 fast-ext shortcut for "we have every piece" — sent in place
    /// of a full Bitfield when both sides support fast-ext and we are a
    /// complete seed.
    SendHaveAll,
    /// BEP 6 fast-ext shortcut for "we have nothing yet".
    SendHaveNone,
    /// Choke this peer (stop serving their requests).
    Choke,
    /// Unchoke this peer (start serving).
    Unchoke,
    /// Send a completed block back to the peer (M2 upload path).
    BlockReady {
        /// The request being fulfilled.
        req: BlockRequest,
        /// Block payload.
        data: Bytes,
    },
    /// Tell the peer we won't serve this request (BEP 6).
    RejectRequest(BlockRequest),
    /// Shut the peer task down.
    Shutdown,
}
