//! BEP 9 `ut_metadata` message codec.
//!
//! Metadata exchange allows peers to fetch torrent metadata (the `info`
//! dictionary) directly from other peers, enabling magnet-link downloads without
//! a `.torrent` file.
//!
//! The metadata is split into [`METADATA_PIECE_SIZE`] (16 KiB) pieces. Three
//! message types exist:
//!
//! - **Request** (`msg_type` 0): ask a peer for a metadata piece.
//! - **Data** (`msg_type` 1): deliver a metadata piece. The bencoded dict is
//!   followed by the raw metadata bytes for that piece.
//! - **Reject** (`msg_type` 2): the peer does not have the metadata.
//!
//! This module handles encoding and decoding only — session-level assembly and
//! verification live in higher layers.

use std::borrow::Cow;
use std::collections::BTreeMap;

use bytes::Bytes;

use magpie_bt_bencode::{self as bencode, Value};

/// BEP 9 metadata piece size (16 KiB).
pub const METADATA_PIECE_SIZE: usize = 16_384;

/// Maximum allowed total metadata size (16 MB, matching libtorrent-rasterbar).
pub const MAX_METADATA_SIZE: u64 = 16_000_000;

/// Number of 16 KiB pieces needed to transfer `total_size` bytes of metadata.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // piece count is bounded by MAX_METADATA_SIZE
pub const fn metadata_piece_count(total_size: u64) -> u32 {
    total_size.div_ceil(METADATA_PIECE_SIZE as u64) as u32
}

/// BEP 9 `ut_metadata` message types.
///
/// These messages are carried inside [`Message::Extended`](crate::Message::Extended)
/// with the negotiated `ut_metadata` extension id. The extension id byte is
/// already stripped by the wire codec — [`MetadataMessage::decode`] operates on
/// the raw payload that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMessage {
    /// Request metadata piece `piece` from the peer.
    Request {
        /// Zero-based metadata piece index.
        piece: u32,
    },
    /// Metadata piece payload. `data` contains the raw metadata bytes for this
    /// piece. `total_size` is the total metadata length in bytes.
    Data {
        /// Zero-based metadata piece index.
        piece: u32,
        /// Total metadata size in bytes (across all pieces).
        total_size: u64,
        /// Raw metadata bytes for this piece.
        data: Bytes,
    },
    /// Peer rejected our request for `piece` (they don't have the metadata).
    Reject {
        /// Zero-based metadata piece index that was rejected.
        piece: u32,
    },
}

/// Errors produced when decoding a BEP 9 `ut_metadata` message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// The bencode payload could not be parsed.
    #[error("bencode decode error: {0}")]
    Decode(String),
    /// A required dictionary field is missing.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// The `msg_type` value is not 0, 1, or 2.
    #[error("invalid msg_type: {0}")]
    InvalidMsgType(i64),
    /// The `total_size` exceeds the safety cap.
    #[error("total_size {0} exceeds maximum {MAX_METADATA_SIZE}")]
    TotalSizeTooLarge(u64),
}

impl MetadataMessage {
    /// Decode a BEP 9 `ut_metadata` message from the raw extended payload.
    ///
    /// `payload` is the bytes after the extension-id byte has been stripped by
    /// the wire codec (i.e. the content of
    /// [`Message::Extended { payload, .. }`](crate::Message::Extended)).
    ///
    /// For `msg_type == 1` (data), the raw metadata bytes are appended *after*
    /// the bencoded dictionary. This method uses [`bencode::decode_prefix`] to
    /// locate the boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataError`] if the payload cannot be parsed, required
    /// fields are missing, or constraint checks fail.
    pub fn decode(payload: &[u8]) -> Result<Self, MetadataError> {
        let (value, remainder) =
            bencode::decode_prefix(payload).map_err(|e| MetadataError::Decode(e.to_string()))?;

        let dict = value
            .as_dict()
            .ok_or_else(|| MetadataError::Decode("expected dict".into()))?;

        let msg_type = dict
            .get(&b"msg_type"[..])
            .and_then(Value::as_int)
            .ok_or(MetadataError::MissingField("msg_type"))?;

        let piece = dict
            .get(&b"piece"[..])
            .and_then(Value::as_int)
            .ok_or(MetadataError::MissingField("piece"))?;

        // `piece` is encoded as i64 in bencode; reject negative values and
        // convert to u32.
        let piece = u32::try_from(piece)
            .map_err(|_| MetadataError::Decode(format!("piece index out of range: {piece}")))?;

        match msg_type {
            0 => Ok(Self::Request { piece }),
            1 => {
                let total_size = dict
                    .get(&b"total_size"[..])
                    .and_then(Value::as_int)
                    .ok_or(MetadataError::MissingField("total_size"))?;

                let total_size = u64::try_from(total_size).map_err(|_| {
                    MetadataError::Decode(format!("total_size out of range: {total_size}"))
                })?;

                if total_size > MAX_METADATA_SIZE {
                    return Err(MetadataError::TotalSizeTooLarge(total_size));
                }

                if remainder.len() > METADATA_PIECE_SIZE {
                    return Err(MetadataError::Decode(format!(
                        "data payload {} bytes exceeds piece size limit {}",
                        remainder.len(),
                        METADATA_PIECE_SIZE
                    )));
                }

                let data = Bytes::copy_from_slice(remainder);
                Ok(Self::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            2 => Ok(Self::Reject { piece }),
            _ => Err(MetadataError::InvalidMsgType(msg_type)),
        }
    }

    /// Encode this message into a byte vector suitable for use as the payload
    /// of a [`Message::Extended`](crate::Message::Extended).
    ///
    /// For [`MetadataMessage::Data`], the bencoded dictionary is followed by the
    /// raw metadata bytes, as BEP 9 specifies.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (msg_type, piece, total_size, trailing) = match self {
            Self::Request { piece } => (0i64, *piece, None, &[][..]),
            Self::Data {
                piece,
                total_size,
                data,
            } => (1, *piece, Some(*total_size), data.as_ref()),
            Self::Reject { piece } => (2, *piece, None, &[][..]),
        };

        let mut dict = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(&b"msg_type"[..]),
            Value::Int(msg_type),
        );
        dict.insert(
            Cow::Borrowed(&b"piece"[..]),
            Value::Int(i64::from(piece)),
        );
        if let Some(ts) = total_size {
            dict.insert(
                Cow::Borrowed(&b"total_size"[..]),
                Value::Int(ts.cast_signed()),
            );
        }

        let value = Value::Dict(dict);
        let mut buf = bencode::encode(&value);
        buf.extend_from_slice(trailing);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- round-trip tests ----

    #[test]
    fn round_trip_request() {
        let msg = MetadataMessage::Request { piece: 7 };
        let encoded = msg.encode();
        let decoded = MetadataMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn round_trip_data() {
        let data = Bytes::from_static(b"hello metadata piece");
        let msg = MetadataMessage::Data {
            piece: 3,
            total_size: 50_000,
            data,
        };
        let encoded = msg.encode();
        let decoded = MetadataMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn round_trip_reject() {
        let msg = MetadataMessage::Reject { piece: 42 };
        let encoded = msg.encode();
        let decoded = MetadataMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    // ---- decode-specific tests ----

    #[test]
    fn decode_request() {
        // Hand-crafted bencode: d8:msg_typei0e5:piecei4ee
        let payload = b"d8:msg_typei0e5:piecei4ee";
        let msg = MetadataMessage::decode(payload).unwrap();
        assert_eq!(msg, MetadataMessage::Request { piece: 4 });
    }

    #[test]
    fn decode_data_with_trailing_bytes() {
        // The key BEP 9 quirk: raw metadata bytes follow the bencoded dict.
        let dict_part = b"d8:msg_typei1e5:piecei0e10:total_sizei32000ee";
        let trailing = b"raw metadata bytes here!";
        let mut payload = Vec::from(&dict_part[..]);
        payload.extend_from_slice(trailing);

        let msg = MetadataMessage::decode(&payload).unwrap();
        assert_eq!(
            msg,
            MetadataMessage::Data {
                piece: 0,
                total_size: 32_000,
                data: Bytes::from_static(b"raw metadata bytes here!"),
            }
        );
    }

    #[test]
    fn decode_reject() {
        let payload = b"d8:msg_typei2e5:piecei10ee";
        let msg = MetadataMessage::decode(payload).unwrap();
        assert_eq!(msg, MetadataMessage::Reject { piece: 10 });
    }

    #[test]
    fn decode_data_with_empty_trailing() {
        // Last piece scenario: data present but zero-length trailing bytes.
        let dict_part = b"d8:msg_typei1e5:piecei5e10:total_sizei81920ee";
        let msg = MetadataMessage::decode(dict_part).unwrap();
        assert_eq!(
            msg,
            MetadataMessage::Data {
                piece: 5,
                total_size: 81_920,
                data: Bytes::new(),
            }
        );
    }

    // ---- error tests ----

    #[test]
    fn missing_msg_type() {
        let payload = b"d5:piecei0ee";
        let err = MetadataMessage::decode(payload).unwrap_err();
        assert!(matches!(err, MetadataError::MissingField("msg_type")));
    }

    #[test]
    fn missing_piece() {
        let payload = b"d8:msg_typei0ee";
        let err = MetadataMessage::decode(payload).unwrap_err();
        assert!(matches!(err, MetadataError::MissingField("piece")));
    }

    #[test]
    fn invalid_msg_type() {
        let payload = b"d8:msg_typei99e5:piecei0ee";
        let err = MetadataMessage::decode(payload).unwrap_err();
        assert!(matches!(err, MetadataError::InvalidMsgType(99)));
    }

    #[test]
    fn total_size_too_large() {
        // 20_000_000 > MAX_METADATA_SIZE (16_000_000)
        let payload = b"d8:msg_typei1e5:piecei0e10:total_sizei20000000ee";
        let err = MetadataMessage::decode(payload).unwrap_err();
        assert!(matches!(err, MetadataError::TotalSizeTooLarge(20_000_000)));
    }

    #[test]
    fn data_payload_exceeds_piece_size() {
        // Build a Data message with trailing bytes exceeding METADATA_PIECE_SIZE.
        let dict_part = b"d8:msg_typei1e5:piecei0e10:total_sizei32000ee";
        let oversized_trailing = vec![0xABu8; METADATA_PIECE_SIZE + 1];
        let mut payload = Vec::from(&dict_part[..]);
        payload.extend_from_slice(&oversized_trailing);

        let err = MetadataMessage::decode(&payload).unwrap_err();
        match err {
            MetadataError::Decode(msg) => {
                assert!(
                    msg.contains("exceeds piece size limit"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }

    #[test]
    fn data_missing_total_size() {
        let payload = b"d8:msg_typei1e5:piecei0ee";
        let err = MetadataMessage::decode(payload).unwrap_err();
        assert!(matches!(err, MetadataError::MissingField("total_size")));
    }

    // ---- metadata_piece_count ----

    #[test]
    fn piece_count_zero() {
        assert_eq!(metadata_piece_count(0), 0);
    }

    #[test]
    fn piece_count_one_byte() {
        assert_eq!(metadata_piece_count(1), 1);
    }

    #[test]
    fn piece_count_just_under_boundary() {
        assert_eq!(metadata_piece_count(16_383), 1);
    }

    #[test]
    fn piece_count_exact_boundary() {
        assert_eq!(metadata_piece_count(16_384), 1);
    }

    #[test]
    fn piece_count_just_over_boundary() {
        assert_eq!(metadata_piece_count(16_385), 2);
    }

    #[test]
    fn piece_count_large() {
        // 1_000_000 / 16_384 = 61.035… → 62 pieces
        assert_eq!(metadata_piece_count(1_000_000), 62);
    }
}
