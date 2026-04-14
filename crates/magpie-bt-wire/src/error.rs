//! Typed errors for peer wire framing.

use thiserror::Error;

/// Error returned when encoding or decoding a peer wire frame.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WireError {
    /// A length-prefixed frame announced a payload larger than the configured
    /// per-message ceiling. Defends against attacker-controlled allocations.
    #[error("frame payload {len} exceeds configured maximum {max}")]
    PayloadTooLarge {
        /// Announced payload length, in bytes (excluding the 4-byte length prefix).
        len: u32,
        /// Configured maximum payload size, in bytes.
        max: u32,
    },
    /// A message id byte was not recognised. The frame has been consumed; the
    /// stream may still be salvageable but typically the connection is closed.
    #[error("unknown message id {id}")]
    UnknownId {
        /// The unrecognised id byte.
        id: u8,
    },
    /// A recognised message id arrived with a payload size that does not match
    /// the BEP 3 / BEP 6 encoding for that message.
    #[error("malformed message id {id}: payload length {len} invalid")]
    MalformedPayload {
        /// The message id whose payload was malformed.
        id: u8,
        /// The actual payload length that was rejected.
        len: usize,
    },
    /// Handshake `pstrlen` byte did not equal the BEP 3 value of 19.
    #[error("handshake pstrlen {0} is not 19")]
    BadHandshakePstrlen(u8),
    /// Handshake protocol string was not the literal `"BitTorrent protocol"`.
    #[error("handshake protocol string mismatch")]
    BadHandshakePstr,
    /// Underlying I/O error surfaced by the codec when wired into a stream.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
