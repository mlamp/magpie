//! Peer wire message variants per BEP 3 and BEP 6.
//!
//! BEP 10 extension messages are surfaced as the opaque
//! [`Message::Extended`] variant — payload framing is left to higher layers.

use bytes::Bytes;

use crate::block::{Block, BlockRequest};

/// Wire-format message ids.
#[allow(missing_docs)]
pub mod id {
    // BEP 3
    pub const CHOKE: u8 = 0;
    pub const UNCHOKE: u8 = 1;
    pub const INTERESTED: u8 = 2;
    pub const NOT_INTERESTED: u8 = 3;
    pub const HAVE: u8 = 4;
    pub const BITFIELD: u8 = 5;
    pub const REQUEST: u8 = 6;
    pub const PIECE: u8 = 7;
    pub const CANCEL: u8 = 8;
    // BEP 6
    pub const SUGGEST_PIECE: u8 = 0x0D;
    pub const HAVE_ALL: u8 = 0x0E;
    pub const HAVE_NONE: u8 = 0x0F;
    pub const REJECT_REQUEST: u8 = 0x10;
    pub const ALLOWED_FAST: u8 = 0x11;
    // BEP 10
    pub const EXTENDED: u8 = 20;
}

/// A decoded peer wire message.
///
/// Bitfield and extended payloads are held as refcounted [`Bytes`], so cloning
/// a `Message` is O(1) regardless of payload size.
///
/// # Caller responsibilities
///
/// The codec is intentionally stateless and does not know the torrent's piece
/// count or whether each side negotiated optional capabilities. Sessions MUST
/// enforce the following on inbound messages:
///
/// - **`Bitfield`** (W3): payload length must equal `ceil(piece_count / 8)`,
///   and any spare bits in the final byte must be zero (BEP 3). Drop the
///   connection if either invariant is violated.
/// - **BEP 6 messages** (W4) — `HaveAll`, `HaveNone`, `SuggestPiece`,
///   `RejectRequest`, `AllowedFast`: only valid if both peers set the Fast
///   extension bit in the [`Handshake`](crate::Handshake) reserved field. If
///   the handshake did not negotiate Fast support, treat any of these messages
///   as a protocol violation and close the connection.
/// - **`Extended`** (BEP 10): the embedded `id` is opaque to this crate.
///   Sessions must dispatch on the extension-handshake `m` map.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// Empty 4-byte length prefix; sent periodically to keep idle TCP
    /// connections open.
    KeepAlive,
    /// Sender will not service requests from this peer until an `Unchoke`.
    Choke,
    /// Sender is willing to service requests from this peer.
    Unchoke,
    /// Sender wants to download from this peer.
    Interested,
    /// Sender no longer wants to download from this peer.
    NotInterested,
    /// Sender now has the named piece.
    Have(u32),
    /// Sender's bitfield. Length must equal `ceil(piece_count / 8)`; spare
    /// bits in the final byte must be zero. The codec does not know the piece
    /// count and therefore cannot enforce either invariant — see the enum-level
    /// "Caller responsibilities" note.
    Bitfield(Bytes),
    /// Request a block.
    Request(BlockRequest),
    /// Block payload responding to an earlier [`Request`](Message::Request).
    Piece(Block),
    /// Withdraw a previously sent request.
    Cancel(BlockRequest),
    /// BEP 6: sender has every piece. Replaces the initial bitfield.
    HaveAll,
    /// BEP 6: sender has no pieces. Replaces the initial bitfield.
    HaveNone,
    /// BEP 6: hint that this piece would be useful next.
    SuggestPiece(u32),
    /// BEP 6: explicit rejection of a prior request.
    RejectRequest(BlockRequest),
    /// BEP 6: peer may request this piece even while choked (allowed-fast set).
    AllowedFast(u32),
    /// BEP 10: opaque extension-protocol message. The first byte of the
    /// original payload becomes [`id`](Message::Extended::id); the rest is
    /// `payload`.
    Extended {
        /// Extension message id (0 for the extension-handshake; otherwise the
        /// id negotiated via the handshake's `m` map).
        id: u8,
        /// Bencoded extension payload (opaque to this crate).
        payload: Bytes,
    },
}
