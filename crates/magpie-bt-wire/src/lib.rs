//! Peer wire protocol codec for the magpie BitTorrent library.
//!
//! Implements:
//!
//! - The BEP 3 [handshake](crate::Handshake) and the BEP 3 message set
//!   ([`Choke`](Message::Choke), [`Have`](Message::Have),
//!   [`Bitfield`](Message::Bitfield), [`Request`](Message::Request),
//!   [`Piece`](Message::Piece), [`Cancel`](Message::Cancel), …).
//! - The BEP 6 Fast extension messages
//!   ([`HaveAll`](Message::HaveAll), [`HaveNone`](Message::HaveNone),
//!   [`SuggestPiece`](Message::SuggestPiece),
//!   [`RejectRequest`](Message::RejectRequest),
//!   [`AllowedFast`](Message::AllowedFast)).
//! - The BEP 10 extension-protocol envelope as the opaque
//!   [`Extended`](Message::Extended) variant; payload framing is left to
//!   higher layers.
//!
//! The codec is I/O-free: [`WireCodec`] implements
//! [`tokio_util::codec::Decoder`] and `Encoder`, so wiring it onto a
//! `TcpStream` is a one-liner via
//! [`tokio_util::codec::Framed`]. Bitfield and
//! piece payloads are held as refcounted [`bytes::Bytes`], so cloning a
//! [`Message`] never copies the payload.
//!
//! # Example
//!
//! ```
//! use bytes::BytesMut;
//! use magpie_bt_wire::{BlockRequest, Message, WireCodec};
//! use tokio_util::codec::{Decoder, Encoder};
//!
//! let mut codec = WireCodec::default();
//! let mut buf = BytesMut::new();
//! codec
//!     .encode(Message::Request(BlockRequest::new(0, 0, 16 * 1024)), &mut buf)
//!     .unwrap();
//!
//! // Round-trip the framed bytes back through the decoder.
//! let decoded = codec.decode(&mut buf).unwrap().unwrap();
//! assert_eq!(decoded, Message::Request(BlockRequest::new(0, 0, 16 * 1024)));
//! ```
#![forbid(unsafe_code)]

mod block;
mod codec;
mod error;
pub mod extension;
mod handshake;
mod message;
pub mod metadata;
pub mod pex;

pub use block::{BLOCK_SIZE, Block, BlockRequest};
pub use codec::{DEFAULT_MAX_PAYLOAD, WireCodec};
pub use error::WireError;
pub use extension::{ExtensionError, ExtensionHandshake, ExtensionRegistry, MAX_EXTENSIONS};
pub use handshake::{HANDSHAKE_LEN, Handshake, PSTR, PSTRLEN};
pub use message::{Message, id};
pub use metadata::{
    MAX_METADATA_SIZE, METADATA_PIECE_SIZE, MetadataError, MetadataMessage, metadata_piece_count,
};
pub use pex::{MAX_PEX_PEERS, PexError, PexFlags, PexMessage, PexPeer};
