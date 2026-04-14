//! Wire-protocol step definitions covering BEP 3 and BEP 6 scenarios.
//!
//! `&mut MagpieWorld` is the cucumber-imposed signature; clippy's
//! `needless_pass_by_ref_mut` and `needless_pass_by_value` would have us drop
//! the borrow / take by reference, neither of which the framework supports.
#![allow(clippy::needless_pass_by_ref_mut, clippy::needless_pass_by_value,
    clippy::too_many_lines, clippy::redundant_clone, clippy::used_underscore_binding)]

use bytes::{Bytes, BytesMut};
use cucumber::{given, then, when};
use magpie_bt_wire::{
    Block, BlockRequest, Handshake, Message, WireCodec, id,
};
use tokio_util::codec::{Decoder, Encoder};

use crate::MagpieWorld;

fn parse_hex(bytes: &str) -> Vec<u8> {
    bytes
        .split_whitespace()
        .map(|s| u8::from_str_radix(s, 16).expect("hex byte"))
        .collect()
}

// ---- BEP 3: handshake ----

#[given(regex = r#"^a handshake with info-hash "(.+)" repeated 20 times and peer-id "(.+)" repeated 20 times$"#)]
fn handshake_repeating(world: &mut MagpieWorld, hash_byte: String, peer_byte: String) {
    let hi = u8::from_str_radix(&hash_byte, 16).expect("hex hash byte");
    let pi = u8::from_str_radix(&peer_byte, 16).expect("hex peer byte");
    world.handshake = Some(Handshake::new([hi; 20], [pi; 20]));
}

#[given("a handshake with the Fast extension bit set")]
fn handshake_fast(world: &mut MagpieWorld) {
    world.handshake = Some(Handshake::new([0xAA; 20], [0xBB; 20]).with_fast_ext());
}

#[when("the handshake is encoded then decoded")]
fn handshake_roundtrip(world: &mut MagpieWorld) {
    let h = world.handshake.expect("handshake set");
    let bytes = h.to_bytes();
    world.decoded_handshake = Some(Handshake::decode(&bytes).expect("decode"));
}

#[then("the decoded info-hash matches and the Fast extension bit is unset")]
fn handshake_decoded_default(world: &mut MagpieWorld) {
    let original = world.handshake.expect("handshake set");
    let back = world.decoded_handshake.expect("handshake decoded");
    assert_eq!(original, back);
    assert!(!back.supports_fast_ext());
}

#[then("the Fast extension bit is set")]
fn handshake_fast_set(world: &mut MagpieWorld) {
    let back = world.decoded_handshake.expect("handshake decoded");
    assert!(back.supports_fast_ext());
}

// ---- BEP 3 / BEP 6: message round-trips ----

#[given(regex = r"^a Have message for piece (\d+)$")]
fn given_have(world: &mut MagpieWorld, piece: u32) {
    world.pending_message = Some(Message::Have(piece));
}

#[given(regex = r"^a Request for piece (\d+), offset (\d+), length (\d+)$")]
fn given_request(world: &mut MagpieWorld, piece: u32, offset: u32, length: u32) {
    world.pending_message = Some(Message::Request(BlockRequest::new(piece, offset, length)));
}

#[given(regex = r#"^a Bitfield containing bytes "(.+)"$"#)]
fn given_bitfield(world: &mut MagpieWorld, bytes: String) {
    let raw = parse_hex(&bytes);
    world.pending_message = Some(Message::Bitfield(Bytes::from(raw)));
}

#[given("a HaveAll message")]
fn given_have_all(world: &mut MagpieWorld) {
    world.pending_message = Some(Message::HaveAll);
}

#[given("a HaveNone message")]
fn given_have_none(world: &mut MagpieWorld) {
    world.pending_message = Some(Message::HaveNone);
}

#[given(regex = r"^an AllowedFast message for piece (\d+)$")]
fn given_allowed_fast(world: &mut MagpieWorld, piece: u32) {
    world.pending_message = Some(Message::AllowedFast(piece));
}

#[given(regex = r"^a RejectRequest for piece (\d+), offset (\d+), length (\d+)$")]
fn given_reject(world: &mut MagpieWorld, piece: u32, offset: u32, length: u32) {
    world.pending_message = Some(Message::RejectRequest(BlockRequest::new(piece, offset, length)));
}

#[when("the wire codec encodes and decodes it")]
fn codec_roundtrip(world: &mut MagpieWorld) {
    let msg = world.pending_message.clone().expect("pending message");
    let mut codec = WireCodec::default();
    let mut buf = BytesMut::new();
    codec.encode(msg.clone(), &mut buf).expect("encode");
    let decoded = codec.decode(&mut buf).expect("decode").expect("complete frame");
    assert!(buf.is_empty(), "frame should be fully consumed");
    world.decoded_message = Some(decoded);
}

#[when("the wire codec encodes it")]
fn codec_encode_only(world: &mut MagpieWorld) {
    let msg = world.pending_message.clone().expect("pending message");
    let mut codec = WireCodec::default();
    let mut buf = BytesMut::new();
    codec.encode(msg, &mut buf).expect("encode");
    world.encoded_buf = buf;
}

#[then(regex = r"^the decoded piece index is (\d+)$")]
fn then_have_piece(world: &mut MagpieWorld, piece: u32) {
    match world.last_decoded() {
        Message::Have(p) => assert_eq!(*p, piece),
        other => panic!("expected Have, got {other:?}"),
    }
}

#[then(regex = r"^the resulting frame begins with length-prefix (\d+) and message id (\d+)$")]
fn then_frame_layout(world: &mut MagpieWorld, len: u32, id_byte: u8) {
    let buf = &world.encoded_buf;
    assert!(buf.len() >= 5, "frame too short");
    let prefix = u32::from_be_bytes(buf[..4].try_into().unwrap());
    assert_eq!(prefix, len, "length prefix mismatch");
    assert_eq!(buf[4], id_byte, "message id mismatch");
    let _ = (id::REQUEST, id::PIECE); // touch ids to silence dead-code
}

#[then(regex = r#"^the decoded bitfield bytes equal "(.+)"$"#)]
fn then_bitfield_eq(world: &mut MagpieWorld, bytes: String) {
    let want = parse_hex(&bytes);
    match world.last_decoded() {
        Message::Bitfield(b) => assert_eq!(&b[..], &want[..]),
        other => panic!("expected Bitfield, got {other:?}"),
    }
}

#[then("the decoded message is HaveAll")]
fn then_have_all(world: &mut MagpieWorld) {
    assert!(matches!(world.last_decoded(), Message::HaveAll));
}

#[then("the decoded message is HaveNone")]
fn then_have_none(world: &mut MagpieWorld) {
    assert!(matches!(world.last_decoded(), Message::HaveNone));
}

#[then(regex = r"^the decoded AllowedFast piece index is (\d+)$")]
fn then_allowed_fast(world: &mut MagpieWorld, piece: u32) {
    match world.last_decoded() {
        Message::AllowedFast(p) => assert_eq!(*p, piece),
        other => panic!("expected AllowedFast, got {other:?}"),
    }
}

#[then("the decoded RejectRequest matches the original block address")]
fn then_reject_matches(world: &mut MagpieWorld) {
    let original = match world.pending_message.clone().unwrap() {
        Message::RejectRequest(r) => r,
        other => panic!("pending was not RejectRequest: {other:?}"),
    };
    match world.last_decoded() {
        Message::RejectRequest(r) => assert_eq!(*r, original),
        other => panic!("expected RejectRequest, got {other:?}"),
    }
}

// Touch unused symbols to satisfy unused-import lint when scenarios are
// trimmed.
#[allow(dead_code)]
fn _touch() {
    let _ = Block::new(0, 0, Bytes::new());
}
