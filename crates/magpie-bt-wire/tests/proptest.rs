//! Property tests for magpie-bt-wire.
//!
//! Two properties guard the codec:
//! 1. Any well-formed [`Message`] survives encode → decode.
//! 2. Arbitrary byte streams fed to the decoder never panic.
#![allow(missing_docs)]

use bytes::{Bytes, BytesMut};
use magpie_bt_wire::{Block, BlockRequest, Handshake, Message, WireCodec};
use proptest::prelude::*;
use tokio_util::codec::{Decoder, Encoder};

fn arb_block_request() -> impl Strategy<Value = BlockRequest> {
    (any::<u32>(), any::<u32>(), 1u32..=(1 << 17))
        .prop_map(|(p, o, l)| BlockRequest::new(p, o, l))
}

fn arb_message() -> impl Strategy<Value = Message> {
    let payload_bytes = prop::collection::vec(any::<u8>(), 0..1024);
    let block_payload = prop::collection::vec(any::<u8>(), 0..16 * 1024 + 1);
    prop_oneof![
        Just(Message::KeepAlive),
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::NotInterested),
        Just(Message::HaveAll),
        Just(Message::HaveNone),
        any::<u32>().prop_map(Message::Have),
        any::<u32>().prop_map(Message::SuggestPiece),
        any::<u32>().prop_map(Message::AllowedFast),
        arb_block_request().prop_map(Message::Request),
        arb_block_request().prop_map(Message::Cancel),
        arb_block_request().prop_map(Message::RejectRequest),
        payload_bytes.clone().prop_map(|v| Message::Bitfield(Bytes::from(v))),
        (any::<u32>(), any::<u32>(), block_payload).prop_map(|(p, o, d)| {
            Message::Piece(Block::new(p, o, Bytes::from(d)))
        }),
        (any::<u8>(), payload_bytes).prop_map(|(id, p)| Message::Extended {
            id,
            payload: Bytes::from(p),
        }),
    ]
}

fn arb_handshake() -> impl Strategy<Value = Handshake> {
    (
        any::<[u8; 8]>(),
        any::<[u8; 20]>(),
        any::<[u8; 20]>(),
    )
        .prop_map(|(reserved, info_hash, peer_id)| Handshake {
            reserved,
            info_hash,
            peer_id,
        })
}

proptest! {
    #[test]
    fn message_encode_decode_roundtrip(msg in arb_message()) {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();
        let back = codec.decode(&mut buf).unwrap().expect("complete frame");
        prop_assert!(buf.is_empty());
        prop_assert_eq!(back, msg);
    }

    #[test]
    fn handshake_roundtrip(h in arb_handshake()) {
        let bytes = h.to_bytes();
        let back = Handshake::decode(&bytes).unwrap();
        prop_assert_eq!(h, back);
    }

    #[test]
    fn decoder_never_panics_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let mut codec = WireCodec::default();
        let mut buf = BytesMut::from(&bytes[..]);
        // Drain any complete frames; ignore success/failure — what matters is no panic.
        for _ in 0..16 {
            match codec.decode(&mut buf) {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    #[test]
    fn decoder_never_panics_on_arbitrary_handshake(bytes in prop::array::uniform32(any::<u8>())) {
        // Stretch 32 bytes deterministically into a 68-byte handshake-shaped buffer.
        let mut h = [0u8; 68];
        for (i, b) in h.iter_mut().enumerate() {
            *b = bytes[i % 32];
        }
        let _ = Handshake::decode(&h);
    }
}
