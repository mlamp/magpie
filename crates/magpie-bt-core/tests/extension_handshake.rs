//! Integration test: BEP 10 extension handshake over a duplex stream.
//!
//! Spawns a `PeerConn` with `extension_protocol: true` on one side of a
//! `tokio::io::duplex`, drives the other side manually, and verifies:
//!
//! 1. The `PeerConn` sends its own extension handshake (Extended id=0).
//! 2. The session receives `PeerToSession::ExtensionHandshake` with the
//!    peer's advertised extensions.
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::needless_collect,
    clippy::too_many_lines,
    clippy::significant_drop_tightening
)]

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use magpie_bt_wire::ExtensionHandshake;
use magpie_bt_core::session::{
    HandshakeRole, PeerConfig, PeerConn, PeerSlot, PeerToSession, PEER_TO_SESSION_CAPACITY,
    perform_handshake,
};
use magpie_bt_wire::{Message, WireCodec};
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_handshake_exchanged() {
    let info_hash = [0xAAu8; 20];
    let our_peer_id = *b"-Mg0001-exttest00000";
    let remote_peer_id = *b"-Mg0001-exttest11111";
    let slot = PeerSlot(42);

    let (peer_to_session_tx, mut peer_to_session_rx) = mpsc::channel(PEER_TO_SESSION_CAPACITY);
    let (_session_to_peer_tx, session_to_peer_rx) = mpsc::unbounded_channel();

    let peer_config = PeerConfig {
        peer_id: our_peer_id,
        info_hash,
        fast_ext: true,
        extension_protocol: true,
        max_in_flight: 4,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_secs(5),
        extension_handshake_timeout: Duration::from_secs(5), remote_addr: None, metadata_size: None, local_listen_port: None,
    };

    let (peer_io, mut remote_io) = duplex(64 * 1024);

    // Spawn the PeerConn side.
    let handshake_cfg = peer_config.clone();
    let peer_task = tokio::spawn(async move {
        let mut io = peer_io;
        let remote = perform_handshake(&mut io, &handshake_cfg, HandshakeRole::Initiator)
            .await
            .expect("PeerConn handshake");
        let conn = PeerConn::new(io, slot, handshake_cfg, peer_to_session_tx, session_to_peer_rx);
        conn.run(remote).await;
    });

    // Drive the remote side: perform BEP 3 handshake with extension protocol
    // bit set.
    let remote_cfg = PeerConfig {
        peer_id: remote_peer_id,
        info_hash,
        fast_ext: true,
        extension_protocol: true,
        max_in_flight: 0,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_secs(5),
        extension_handshake_timeout: Duration::from_secs(5), remote_addr: None, metadata_size: None, local_listen_port: None,
    };
    let _remote_hs = perform_handshake(&mut remote_io, &remote_cfg, HandshakeRole::Responder)
        .await
        .expect("remote handshake");

    let mut framed = Framed::new(remote_io, WireCodec::new(256 * 1024));

    // The PeerConn should send its extension handshake (Extended id=0).
    let msg = tokio::time::timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timeout waiting for PeerConn ext handshake")
        .expect("stream ended")
        .expect("frame error");

    let their_payload = match msg {
        Message::Extended { id: 0, payload } => payload,
        other => panic!("expected Extended(id=0), got {other:?}"),
    };

    // Decode and verify our PeerConn's handshake.
    let their_hs = ExtensionHandshake::decode(&their_payload).expect("decode PeerConn handshake");
    assert_eq!(their_hs.extensions.get("ut_metadata"), Some(&1u8));
    assert_eq!(their_hs.extensions.get("ut_pex"), Some(&2u8));
    assert_eq!(their_hs.client.as_deref(), Some("magpie"));

    // Now send our remote extension handshake back.
    let remote_ext_hs = ExtensionHandshake {
        extensions: [("ut_metadata".into(), 10u8), ("ut_pex".into(), 11u8)]
            .into_iter()
            .collect(),
        metadata_size: Some(65536),
        client: Some("test-peer".into()),
        listen_port: None,
        yourip: None,
        reqq: None,
    };
    let payload = Bytes::from(remote_ext_hs.encode());
    framed
        .send(Message::Extended { id: 0, payload })
        .await
        .expect("send remote ext handshake");

    // Drain the session channel: expect Connected first, then
    // ExtensionHandshake.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut got_connected = false;
    let mut got_ext_hs = false;
    let mut extensions: HashMap<String, u8> = HashMap::new();
    let mut metadata_size: Option<u64> = None;
    let mut client: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), peer_to_session_rx.recv()).await {
            Ok(Some(PeerToSession::Connected { .. })) => {
                got_connected = true;
            }
            Ok(Some(PeerToSession::ExtensionHandshake {
                extensions: ext,
                metadata_size: ms,
                client: cl,
                ..
            })) => {
                got_ext_hs = true;
                extensions = ext;
                metadata_size = ms;
                client = cl;
                break;
            }
            Ok(Some(_other)) => {
                // Ignore other messages.
            }
            Ok(None) | Err(_) => break,
        }
    }

    assert!(got_connected, "should have received Connected");
    assert!(got_ext_hs, "should have received ExtensionHandshake");
    assert_eq!(extensions.get("ut_metadata"), Some(&10));
    assert_eq!(extensions.get("ut_pex"), Some(&11));
    assert_eq!(metadata_size, Some(65536));
    assert_eq!(client.as_deref(), Some("test-peer"));

    // Clean up: drop the framed stream to trigger EOF on PeerConn.
    drop(framed);
    let _ = tokio::time::timeout(Duration::from_secs(2), peer_task).await;
}
