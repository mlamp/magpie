//! Length-prefixed framing of [`Message`] over an asynchronous byte stream.
//!
//! Implements [`tokio_util::codec::Decoder`] and [`tokio_util::codec::Encoder`]
//! so a [`Framed<TcpStream, WireCodec>`](tokio_util::codec::Framed) yields
//! `Stream<Item = Result<Message, WireError>>` and `Sink<Message>`.
//!
//! The frame is the BEP 3 standard:
//!
//! ```text
//! 0      4              4 + N
//! +------+--------------+
//! | len  | id + payload |
//! +------+--------------+
//!   u32        N bytes
//! ```
//!
//! `len == 0` is the `KeepAlive` heartbeat. All other frames carry a one-byte
//! id followed by an id-specific payload.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::block::{BLOCK_SIZE, Block, BlockRequest};
use crate::error::WireError;
use crate::message::{Message, id};

/// Default per-message ceiling: 256 KiB.
///
/// Sized to fit a bitfield for any torrent with up to ~2 million v1 pieces
/// (typical torrents have <100 k pieces, so this leaves headroom). Sessions
/// that need to handle larger bitfields can call
/// [`WireCodec::set_max_payload`] after parsing the metainfo.
///
/// **Hardening note**: the previous 1 MiB default let a peer that sent only
/// the 4-byte length prefix amplify into a 1 MiB resident buffer per
/// connection (see W1 in the M1 phase 1+2 red-team review). Lowering the
/// default plus enforcing a `Piece`-specific cap of `BLOCK_SIZE + 8`
/// caps the per-peer footprint.
pub const DEFAULT_MAX_PAYLOAD: u32 = 256 * 1024;

/// Hard ceiling for `Piece` message payloads: 8 bytes of header (piece index +
/// offset) plus the standard 16 KiB block. Magpie pins block size at 16 KiB
/// per the v2 invariant in `docs/PROJECT.md`, so anything larger is malformed.
const PIECE_PAYLOAD_MAX: usize = 8 + BLOCK_SIZE as usize;

/// `tokio_util` codec that frames [`Message`] values.
///
/// The decoder enforces a configurable per-message size ceiling
/// ([`WireCodec::new`]); frames above the ceiling fail with
/// [`WireError::PayloadTooLarge`] without allocating the payload.
#[derive(Debug, Clone, Copy)]
pub struct WireCodec {
    max_payload: u32,
}

impl WireCodec {
    /// Construct a codec with the given per-message ceiling.
    #[must_use]
    pub const fn new(max_payload: u32) -> Self {
        Self { max_payload }
    }

    /// Configured per-message ceiling, in bytes.
    #[must_use]
    pub const fn max_payload(&self) -> u32 {
        self.max_payload
    }

    /// Raise (or lower) the per-message ceiling. Sessions typically call this
    /// once the metainfo is parsed, sizing the ceiling to the actual bitfield
    /// length plus a small overhead.
    pub const fn set_max_payload(&mut self, max_payload: u32) {
        self.max_payload = max_payload;
    }
}

impl Default for WireCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PAYLOAD)
    }
}

impl Decoder for WireCodec {
    type Item = Message;
    type Error = WireError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, WireError> {
        if src.len() < 4 {
            // Bounded reserve: just enough to finish reading the length prefix.
            // We deliberately do NOT reserve for the announced payload until we
            // see the bytes arriving — otherwise a peer that sends only the
            // 4-byte prefix can amplify into a `max_payload`-sized allocation
            // per connection (W1 in the phase 1+2 red-team review).
            src.reserve(4 - src.len());
            return Ok(None);
        }
        let len = u32::from_be_bytes(src[..4].try_into().unwrap());
        if len > self.max_payload {
            return Err(WireError::PayloadTooLarge {
                len,
                max: self.max_payload,
            });
        }
        let total = 4 + len as usize;
        if src.len() < total {
            // No `src.reserve(total - src.len())` here — see W1 note above.
            // `Framed` will keep reading; the BytesMut grows as bytes actually
            // arrive, not on attacker promise.
            return Ok(None);
        }
        // Commit: consume length prefix.
        src.advance(4);
        if len == 0 {
            return Ok(Some(Message::KeepAlive));
        }
        let mut frame = src.split_to(len as usize);
        let id_byte = frame[0];
        frame.advance(1);
        let payload = frame.freeze();
        decode_body(id_byte, payload).map(Some)
    }
}

impl Encoder<Message> for WireCodec {
    type Error = WireError;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), WireError> {
        encode_message(&msg, dst, self.max_payload)
    }
}

impl Encoder<&Message> for WireCodec {
    type Error = WireError;

    fn encode(&mut self, msg: &Message, dst: &mut BytesMut) -> Result<(), WireError> {
        encode_message(msg, dst, self.max_payload)
    }
}

fn decode_body(id_byte: u8, payload: bytes::Bytes) -> Result<Message, WireError> {
    let len = payload.len();
    let malformed = || WireError::MalformedPayload { id: id_byte, len };
    match id_byte {
        id::CHOKE => empty(&payload, Message::Choke, id_byte),
        id::UNCHOKE => empty(&payload, Message::Unchoke, id_byte),
        id::INTERESTED => empty(&payload, Message::Interested, id_byte),
        id::NOT_INTERESTED => empty(&payload, Message::NotInterested, id_byte),
        id::HAVE => Ok(Message::Have(read_u32(&payload).ok_or_else(malformed)?)),
        id::BITFIELD => Ok(Message::Bitfield(payload)),
        id::REQUEST => read_block_request(&payload)
            .map(Message::Request)
            .ok_or_else(malformed),
        id::PIECE => {
            // W2: cap Piece payload at `BLOCK_SIZE + 8`. v2 mandates 16 KiB
            // blocks; any peer asking us to buffer more is either buggy or
            // hostile.
            if !(8..=PIECE_PAYLOAD_MAX).contains(&len) {
                return Err(malformed());
            }
            let piece = u32::from_be_bytes(payload[0..4].try_into().unwrap());
            let offset = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            let data = payload.slice(8..);
            Ok(Message::Piece(Block {
                piece,
                offset,
                data,
            }))
        }
        id::CANCEL => read_block_request(&payload)
            .map(Message::Cancel)
            .ok_or_else(malformed),
        id::HAVE_ALL => empty(&payload, Message::HaveAll, id_byte),
        id::HAVE_NONE => empty(&payload, Message::HaveNone, id_byte),
        id::SUGGEST_PIECE => Ok(Message::SuggestPiece(
            read_u32(&payload).ok_or_else(malformed)?,
        )),
        id::REJECT_REQUEST => read_block_request(&payload)
            .map(Message::RejectRequest)
            .ok_or_else(malformed),
        id::ALLOWED_FAST => Ok(Message::AllowedFast(
            read_u32(&payload).ok_or_else(malformed)?,
        )),
        id::EXTENDED => {
            if payload.is_empty() {
                return Err(malformed());
            }
            let ext_id = payload[0];
            let body = payload.slice(1..);
            Ok(Message::Extended {
                id: ext_id,
                payload: body,
            })
        }
        other => Err(WireError::UnknownId { id: other }),
    }
}

fn empty(payload: &bytes::Bytes, msg: Message, id_byte: u8) -> Result<Message, WireError> {
    if payload.is_empty() {
        Ok(msg)
    } else {
        Err(WireError::MalformedPayload {
            id: id_byte,
            len: payload.len(),
        })
    }
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 4 {
        return None;
    }
    Some(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_block_request(bytes: &[u8]) -> Option<BlockRequest> {
    if bytes.len() != 12 {
        return None;
    }
    let piece = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
    let offset = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    Some(BlockRequest {
        piece,
        offset,
        length,
    })
}

fn encode_message(msg: &Message, dst: &mut BytesMut, max_payload: u32) -> Result<(), WireError> {
    fn put_simple(dst: &mut BytesMut, id_byte: u8) {
        dst.reserve(5);
        dst.put_u32(1);
        dst.put_u8(id_byte);
    }
    fn put_u32_msg(dst: &mut BytesMut, id_byte: u8, value: u32) {
        dst.reserve(9);
        dst.put_u32(5);
        dst.put_u8(id_byte);
        dst.put_u32(value);
    }
    fn put_block_request(dst: &mut BytesMut, id_byte: u8, req: &BlockRequest) {
        dst.reserve(17);
        dst.put_u32(13);
        dst.put_u8(id_byte);
        dst.put_u32(req.piece);
        dst.put_u32(req.offset);
        dst.put_u32(req.length);
    }
    fn check_payload(len: usize, max: u32) -> Result<u32, WireError> {
        let n_clamped = u32::try_from(len).unwrap_or(u32::MAX);
        if n_clamped > max || u32::try_from(len).is_err() {
            return Err(WireError::PayloadTooLarge {
                len: n_clamped,
                max,
            });
        }
        Ok(n_clamped)
    }

    match msg {
        Message::KeepAlive => {
            dst.reserve(4);
            dst.put_u32(0);
        }
        Message::Choke => put_simple(dst, id::CHOKE),
        Message::Unchoke => put_simple(dst, id::UNCHOKE),
        Message::Interested => put_simple(dst, id::INTERESTED),
        Message::NotInterested => put_simple(dst, id::NOT_INTERESTED),
        Message::Have(p) => put_u32_msg(dst, id::HAVE, *p),
        Message::Bitfield(b) => {
            let len = check_payload(1 + b.len(), max_payload)?;
            dst.reserve(4 + len as usize);
            dst.put_u32(len);
            dst.put_u8(id::BITFIELD);
            dst.extend_from_slice(b);
        }
        Message::Request(r) => put_block_request(dst, id::REQUEST, r),
        Message::Piece(Block {
            piece,
            offset,
            data,
        }) => {
            let len = check_payload(1 + 8 + data.len(), max_payload)?;
            dst.reserve(4 + len as usize);
            dst.put_u32(len);
            dst.put_u8(id::PIECE);
            dst.put_u32(*piece);
            dst.put_u32(*offset);
            dst.extend_from_slice(data);
        }
        Message::Cancel(r) => put_block_request(dst, id::CANCEL, r),
        Message::HaveAll => put_simple(dst, id::HAVE_ALL),
        Message::HaveNone => put_simple(dst, id::HAVE_NONE),
        Message::SuggestPiece(p) => put_u32_msg(dst, id::SUGGEST_PIECE, *p),
        Message::RejectRequest(r) => put_block_request(dst, id::REJECT_REQUEST, r),
        Message::AllowedFast(p) => put_u32_msg(dst, id::ALLOWED_FAST, *p),
        Message::Extended {
            id: ext_id,
            payload,
        } => {
            let len = check_payload(1 + 1 + payload.len(), max_payload)?;
            dst.reserve(4 + len as usize);
            dst.put_u32(len);
            dst.put_u8(id::EXTENDED);
            dst.put_u8(*ext_id);
            dst.extend_from_slice(payload);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn round_trip(msg: &Message) {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().expect("complete frame");
        assert_eq!(&decoded, msg);
        assert!(buf.is_empty(), "decoder should consume the whole frame");
    }

    #[test]
    fn keepalive_roundtrip() {
        round_trip(&Message::KeepAlive);
    }

    #[test]
    fn simple_messages_roundtrip() {
        for m in [
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::HaveAll,
            Message::HaveNone,
        ] {
            round_trip(&m);
        }
    }

    #[test]
    fn have_roundtrip() {
        round_trip(&Message::Have(42));
        round_trip(&Message::Have(u32::MAX));
    }

    #[test]
    fn bitfield_roundtrip() {
        round_trip(&Message::Bitfield(Bytes::from_static(&[0xFF, 0x0F])));
        round_trip(&Message::Bitfield(Bytes::new()));
    }

    #[test]
    fn block_messages_roundtrip() {
        let req = BlockRequest::new(7, 0x4000, 0x4000);
        round_trip(&Message::Request(req));
        round_trip(&Message::Cancel(req));
        round_trip(&Message::RejectRequest(req));
    }

    #[test]
    fn piece_roundtrip() {
        let payload = Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]);
        round_trip(&Message::Piece(Block::new(3, 16, payload)));
    }

    #[test]
    fn fast_messages_roundtrip() {
        round_trip(&Message::SuggestPiece(99));
        round_trip(&Message::AllowedFast(101));
    }

    #[test]
    fn extended_roundtrip() {
        round_trip(&Message::Extended {
            id: 0,
            payload: Bytes::from_static(b"d1:ai0ee"),
        });
    }

    #[test]
    fn partial_frame_returns_none() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::from(&[0u8, 0, 0][..]); // 3 bytes of length
        assert!(codec.decode(&mut buf).unwrap().is_none());
        // Append rest of length, still no payload.
        buf.extend_from_slice(&[5u8]); // claims 5-byte payload
        assert!(codec.decode(&mut buf).unwrap().is_none());
        // Provide only 2 of the 5 payload bytes.
        buf.extend_from_slice(&[id::HAVE, 0, 0]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        // Complete the frame.
        buf.extend_from_slice(&[0, 7]);
        let m = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(m, Message::Have(7));
    }

    #[test]
    fn rejects_oversized_payload() {
        let mut codec = WireCodec::new(16);
        let mut buf = BytesMut::new();
        buf.put_u32(1024);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            WireError::PayloadTooLarge { len: 1024, max: 16 }
        ));
    }

    #[test]
    fn rejects_unknown_id() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(1);
        buf.put_u8(0xEE);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, WireError::UnknownId { id: 0xEE }));
    }

    #[test]
    fn rejects_malformed_have() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(2); // id + 1 byte: too short for Have
        buf.put_u8(id::HAVE);
        buf.put_u8(0);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            WireError::MalformedPayload {
                id: id::HAVE,
                len: 1
            }
        ));
    }

    #[test]
    fn rejects_choke_with_payload() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(2);
        buf.put_u8(id::CHOKE);
        buf.put_u8(0);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            WireError::MalformedPayload {
                id: id::CHOKE,
                len: 1
            }
        ));
    }

    #[test]
    fn extended_requires_id_byte() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(1);
        buf.put_u8(id::EXTENDED);
        // payload is empty after id byte → malformed
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            WireError::MalformedPayload {
                id: id::EXTENDED,
                ..
            }
        ));
    }

    #[test]
    fn rejects_oversized_piece_payload() {
        // W2: a Piece message larger than 8 + BLOCK_SIZE must be rejected even
        // if the codec ceiling allows it.
        let mut codec = WireCodec::new(1 << 20);
        let mut buf = BytesMut::new();
        let oversized: u32 = BLOCK_SIZE + 9 + 1;
        buf.put_u32(1 + oversized);
        buf.put_u8(id::PIECE);
        buf.put_bytes(0, oversized as usize);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            WireError::MalformedPayload { id: id::PIECE, .. }
        ));
    }

    #[test]
    fn set_max_payload_takes_effect() {
        let mut codec = WireCodec::new(8);
        codec.set_max_payload(64);
        assert_eq!(codec.max_payload(), 64);
    }

    #[test]
    fn default_max_payload_lowered_for_dos_resilience() {
        // W1 hardening: default ceiling pulled down from 1 MiB to 256 KiB.
        assert_eq!(DEFAULT_MAX_PAYLOAD, 256 * 1024);
    }

    #[test]
    fn encodes_keepalive_as_four_zero_bytes() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Message::KeepAlive, &mut buf).unwrap();
        assert_eq!(&buf[..], &[0, 0, 0, 0]);
    }

    #[test]
    fn encodes_request_in_canonical_layout() {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Message::Request(BlockRequest::new(1, 2, 3)), &mut buf)
            .unwrap();
        assert_eq!(
            &buf[..],
            &[
                0,
                0,
                0,
                13, // length = 13
                id::REQUEST,
                0,
                0,
                0,
                1, // piece
                0,
                0,
                0,
                2, // offset
                0,
                0,
                0,
                3, // length
            ]
        );
    }
}
