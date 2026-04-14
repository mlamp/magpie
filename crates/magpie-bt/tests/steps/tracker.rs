//! Tracker / BEP 23 step definitions.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::used_underscore_binding
)]

use cucumber::{given, then, when};
use magpie_bt_core::tracker::{TrackerError, parse_response};

use crate::MagpieWorld;

#[given("a bencoded announce response with interval 1800 and two compact v4 peers")]
fn given_announce_v4(world: &mut MagpieWorld) {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"d8:intervali1800e5:peers12:");
    payload.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE1, 192, 168, 1, 2, 0xC0, 0x35]);
    payload.push(b'e');
    world.announce_bytes = payload;
}

#[given(
    regex = r#"^a bencoded announce response with interval 900 and one compact v6 peer at "(.+)"$"#
)]
fn given_announce_v6(world: &mut MagpieWorld, _addr: String) {
    // Build a minimal v6 list: ::1 + port 6881
    let mut octets = [0u8; 18];
    octets[15] = 1;
    octets[16] = 0x1A;
    octets[17] = 0xE1;
    let mut payload = Vec::new();
    payload.extend_from_slice(b"d8:intervali900e6:peers618:");
    payload.extend_from_slice(&octets);
    payload.push(b'e');
    world.announce_bytes = payload;
}

#[given(regex = r#"^a bencoded tracker response with failure reason "(.+)"$"#)]
fn given_failure(world: &mut MagpieWorld, reason: String) {
    let bytes = format!("d14:failure reason{}:{}e", reason.len(), reason);
    world.announce_bytes = bytes.into_bytes();
}

#[when("the tracker response is parsed")]
fn when_parse(world: &mut MagpieWorld) {
    match parse_response(&world.announce_bytes) {
        Ok(resp) => world.announce_response = Some(resp),
        Err(e) => world.announce_error = Some(e),
    }
}

#[then(regex = r"^the announce interval is (\d+) seconds$")]
fn then_interval(world: &mut MagpieWorld, secs: u64) {
    let resp = world.announce_response.as_ref().expect("response parsed");
    assert_eq!(resp.interval.as_secs(), secs);
}

#[then(regex = r#"^the parsed peer list contains "(.+)"$"#)]
fn then_peer_present(world: &mut MagpieWorld, addr: String) {
    let peers = world.parsed_peers();
    let found = peers.iter().any(|p| p.to_string() == addr);
    assert!(found, "{addr} not in {peers:?}");
}

#[then(regex = r#"^parsing returns a tracker failure with message "(.+)"$"#)]
fn then_failure(world: &mut MagpieWorld, expected: String) {
    let err = world.announce_error.take().expect("expected an error");
    match err {
        TrackerError::Failure(s) => assert_eq!(s, expected),
        other => panic!("expected Failure, got {other:?}"),
    }
}
