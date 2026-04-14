//! Block addressing and payload types shared by `Request`, `Piece`, `Cancel`,
//! and BEP 6 `RejectRequest`.
//!
//! The BitTorrent protocol carves each piece into fixed-size *blocks*. Magpie
//! enforces the v2 invariant of 16 KiB blocks even on v1 transports (see
//! `docs/PROJECT.md`), so consumers can size queues and buffers accordingly.

use bytes::Bytes;

/// Standard block size, 16 KiB. Required by BEP 52 v2 and assumed everywhere
/// in magpie. The final block of the final piece may be shorter.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Address of a block within a torrent.
///
/// Used by [`Message::Request`](crate::Message::Request),
/// [`Message::Cancel`](crate::Message::Cancel), and
/// [`Message::RejectRequest`](crate::Message::RejectRequest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    /// Zero-based piece index.
    pub piece: u32,
    /// Byte offset of the block within the piece.
    pub offset: u32,
    /// Block length in bytes.
    pub length: u32,
}

impl BlockRequest {
    /// Construct a block address.
    #[must_use]
    pub const fn new(piece: u32, offset: u32, length: u32) -> Self {
        Self {
            piece,
            offset,
            length,
        }
    }
}

/// A block payload received from a peer (carried by
/// [`Message::Piece`](crate::Message::Piece)).
///
/// `data` is a refcounted [`Bytes`] slice — cloning the message is O(1) and
/// does not copy the block payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Zero-based piece index.
    pub piece: u32,
    /// Byte offset of the block within the piece.
    pub offset: u32,
    /// Block payload bytes.
    pub data: Bytes,
}

impl Block {
    /// Construct a block payload.
    #[must_use]
    pub const fn new(piece: u32, offset: u32, data: Bytes) -> Self {
        Self {
            piece,
            offset,
            data,
        }
    }
}
