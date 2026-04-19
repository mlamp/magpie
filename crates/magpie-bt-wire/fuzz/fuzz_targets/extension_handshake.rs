#![no_main]
//! Fuzz target: feeding arbitrary bytes into [`ExtensionHandshake::decode`]
//! must never panic. Errors are expected for malformed input.
use libfuzzer_sys::fuzz_target;
use magpie_bt_wire::ExtensionHandshake;

fuzz_target!(|data: &[u8]| {
    // Must not panic regardless of input.
    let _ = ExtensionHandshake::decode(data);
});
