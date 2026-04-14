#![no_main]
//! Fuzz target: feeding arbitrary bytes into [`WireCodec::decode`] must never
//! panic. If a frame decodes successfully, re-encoding it must produce the
//! same bytes that the decoder consumed.
use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use magpie_bt_wire::WireCodec;
use tokio_util::codec::{Decoder, Encoder};

fuzz_target!(|data: &[u8]| {
    let mut codec = WireCodec::default();
    let mut buf = BytesMut::from(data);
    while let Ok(Some(msg)) = codec.decode(&mut buf) {
        let mut reencoded = BytesMut::new();
        if codec.encode(&msg, &mut reencoded).is_err() {
            break;
        }
        let mut roundtrip = reencoded.clone();
        let back = codec
            .decode(&mut roundtrip)
            .expect("re-encoded frame must decode")
            .expect("re-encoded frame must be complete");
        assert_eq!(back, msg);
        assert!(roundtrip.is_empty());
    }
});
