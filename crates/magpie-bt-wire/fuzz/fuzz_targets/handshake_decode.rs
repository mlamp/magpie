#![no_main]
//! Fuzz target: any 68-byte buffer fed to [`Handshake::decode`] must produce
//! a typed error or a successful parse, never a panic.
use libfuzzer_sys::fuzz_target;
use magpie_bt_wire::{HANDSHAKE_LEN, Handshake};

fuzz_target!(|data: &[u8]| {
    if data.len() < HANDSHAKE_LEN {
        return;
    }
    let mut buf = [0u8; HANDSHAKE_LEN];
    buf.copy_from_slice(&data[..HANDSHAKE_LEN]);
    if let Ok(parsed) = Handshake::decode(&buf) {
        let bytes = parsed.to_bytes();
        let back = Handshake::decode(&bytes).expect("re-encode must round-trip");
        assert_eq!(parsed, back);
    }
});
