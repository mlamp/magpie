//! BEP 9 metadata exchange integration tests.
//!
//! Verifies that:
//! 1. A seeder engine serves metadata pieces to a requesting peer.
//! 2. A leecher engine started from a magnet link fetches, verifies, and
//!    transitions to downloading.
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::field_reassign_with_default,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::significant_drop_tightening,
    clippy::unchecked_time_subtraction,
    clippy::similar_names,
    clippy::used_underscore_binding,
    clippy::needless_pass_by_value,
    clippy::needless_continue
)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{
    AddMagnetRequest, AddTorrentRequest, Engine, ListenConfig, MagnetLink,
};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::{HandshakeRole, PeerConfig, TorrentParams, perform_handshake};
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;
use magpie_bt_wire::{Message, MetadataMessage, WireCodec, METADATA_PIECE_SIZE};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

/// Verifies that a seeder engine correctly serves metadata pieces via
/// BEP 9 to a requesting peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_serves_metadata_pieces() {
    let synth = synthetic_torrent_v1("meta_test.bin", 16 * 1024, 2, 99);
    let meta = magpie_bt_metainfo::parse(&synth.torrent).unwrap();
    let info_hash = *meta.info_hash.v1().unwrap();
    let info_bytes = meta.info_bytes.to_vec();

    // Set up seeder engine with pre-populated storage.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(synth.content.len() as u64));
    storage.write_block(0, &synth.content).unwrap();

    let alerts = Arc::new(AlertQueue::new(128));
    alerts.set_mask(AlertCategory::ALL);
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let mut req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: synth.piece_count,
            piece_length: u64::from(synth.piece_length),
            total_length: synth.content.len() as u64,
            piece_hashes: meta.info.v1.as_ref().unwrap().pieces.to_vec(),
            private: false,
        },
        Arc::clone(&storage),
        *b"-Mg0001-seedmeta0001",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.initial_have = vec![true; synth.piece_count as usize];
    req.info_dict_bytes = Some(info_bytes.clone());
    let _tid = engine.add_torrent(req).await.expect("add_torrent");

    // Bind the inbound listener.
    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    // Mock leecher: connect, handshake (with BEP 10), send extension
    // handshake with ut_metadata, request metadata piece 0.
    let mut stream = TcpStream::connect(bound).await.expect("connect");
    let cfg = PeerConfig {
        peer_id: *b"-Mg0001-metaleech001",
        info_hash,
        fast_ext: true,
        extension_protocol: true,
        max_in_flight: 4,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_secs(5),
        extension_handshake_timeout: Duration::from_secs(5),
        remote_addr: None, metadata_size: None, local_listen_port: None,
    };
    let remote_hs = perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator)
        .await
        .expect("handshake");
    assert!(
        remote_hs.supports_extension_protocol(),
        "seeder must advertise BEP 10"
    );

    let mut framed = Framed::new(stream, WireCodec::new(256 * 1024));

    // Send our extension handshake advertising ut_metadata = 3
    let our_ut_id: u8 = 3;
    let ext_hs = build_extension_handshake(our_ut_id);
    framed
        .send(Message::Extended {
            id: 0,
            payload: ext_hs.into(),
        })
        .await
        .expect("send ext handshake");

    // Read the seeder's extension handshake to learn their ut_metadata id.
    let seeder_ut_id = wait_for_extension_handshake(&mut framed).await;

    // Request metadata piece 0.
    let req_msg = MetadataMessage::Request { piece: 0 };
    framed
        .send(Message::Extended {
            id: seeder_ut_id,
            payload: req_msg.encode().into(),
        })
        .await
        .expect("send metadata request");

    // Expect a Data response.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("never received metadata Data response");
        }
        let frame = tokio::time::timeout(Duration::from_millis(500), framed.next()).await;
        match frame {
            Ok(Some(Ok(Message::Extended { id, payload }))) if id == our_ut_id => {
                let msg = MetadataMessage::decode(&payload).expect("decode metadata response");
                match msg {
                    MetadataMessage::Data {
                        piece,
                        total_size,
                        data,
                    } => {
                        assert_eq!(piece, 0);
                        assert_eq!(total_size, info_bytes.len() as u64);
                        // The piece data should match the first METADATA_PIECE_SIZE
                        // bytes (or all of info_bytes if smaller).
                        let expected_end =
                            METADATA_PIECE_SIZE.min(info_bytes.len());
                        assert_eq!(&data[..], &info_bytes[..expected_end]);
                        break;
                    }
                    other => panic!("expected Data, got {other:?}"),
                }
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            Ok(Some(Err(e))) => panic!("wire error: {e:?}"),
            Ok(None) => panic!("eof"),
        }
    }

    engine.shutdown(_tid).await;
    engine.join().await;
}

/// End-to-end test: a leecher engine starts from a magnet link, fetches
/// metadata from a seeder engine, verifies it, transitions to downloading,
/// downloads the content, and verifies SHA-256 integrity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_metadata_fetch_and_download() {
    let synth = synthetic_torrent_v1("magnet_e2e.bin", 16 * 1024, 2, 42);
    let meta = magpie_bt_metainfo::parse(&synth.torrent).unwrap();
    let info_hash = *meta.info_hash.v1().unwrap();
    let info_bytes = meta.info_bytes.to_vec();

    // ---- Seeder setup ----
    let seeder_storage: Arc<dyn Storage> =
        Arc::new(MemoryStorage::new(synth.content.len() as u64));
    seeder_storage.write_block(0, &synth.content).unwrap();

    let seeder_alerts = Arc::new(AlertQueue::new(128));
    seeder_alerts.set_mask(AlertCategory::ALL);
    let seeder_engine = Arc::new(Engine::new(Arc::clone(&seeder_alerts)));

    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: synth.piece_count,
            piece_length: u64::from(synth.piece_length),
            total_length: synth.content.len() as u64,
            piece_hashes: meta.info.v1.as_ref().unwrap().pieces.to_vec(),
            private: false,
        },
        Arc::clone(&seeder_storage),
        *b"-Mg0001-seeder-magn1",
    );
    seed_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    seed_req.initial_have = vec![true; synth.piece_count as usize];
    seed_req.info_dict_bytes = Some(info_bytes.clone());
    let _seed_tid = seeder_engine
        .add_torrent(seed_req)
        .await
        .expect("add_torrent");

    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let seeder_addr = seeder_engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    // ---- Leecher (magnet) setup ----
    let leecher_storage: Arc<dyn Storage> =
        Arc::new(MemoryStorage::new(synth.content.len() as u64));
    let leecher_alerts = Arc::new(AlertQueue::new(128));
    leecher_alerts.set_mask(AlertCategory::ALL);
    let leecher_engine = Arc::new(Engine::new(Arc::clone(&leecher_alerts)));

    let magnet = MagnetLink {
        info_hash,
        trackers: Vec::new(),
        peer_addrs: vec![seeder_addr],
        display_name: Some("magnet_e2e.bin".to_string()),
    };
    let mut mag_req = AddMagnetRequest::new(
        magnet,
        Arc::clone(&leecher_storage),
        *b"-Mg0001-leechmagn001",
    );
    mag_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leecher_tid = leecher_engine
        .add_magnet(mag_req)
        .await
        .expect("add_magnet");

    // Connect the leecher to the seeder.
    leecher_engine
        .add_peer(leecher_tid, seeder_addr)
        .await
        .expect("add_peer");

    // Wait for MetadataReceived and TorrentComplete alerts (up to 30s).
    // Both may arrive in the same drain batch since the download can
    // complete very quickly after metadata is received.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut got_metadata = false;
    let mut completed = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let drained = leecher_alerts.drain();
        for alert in &drained {
            if matches!(alert, Alert::MetadataReceived { torrent } if *torrent == leecher_tid) {
                got_metadata = true;
            }
            if matches!(alert, Alert::TorrentComplete { torrent } if *torrent == leecher_tid) {
                completed = true;
            }
        }
        if got_metadata && completed {
            break;
        }
    }
    assert!(got_metadata, "MetadataReceived alert not received within timeout");
    assert!(completed, "TorrentComplete not received within timeout");

    // Verify content integrity.
    let mut buf = vec![0u8; synth.content.len()];
    leecher_storage.read_block(0, &mut buf).unwrap();
    assert_eq!(
        buf, synth.content,
        "downloaded content mismatch"
    );

    // Cleanup.
    seeder_engine.shutdown(_seed_tid).await;
    seeder_engine.join().await;
    leecher_engine.shutdown(leecher_tid).await;
    leecher_engine.join().await;
}

// --- Helpers ---------------------------------------------------------------

fn build_extension_handshake(ut_metadata_id: u8) -> Vec<u8> {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use magpie_bt_bencode::{Value, encode};

    let mut m = BTreeMap::<Cow<'_, [u8]>, Value<'_>>::new();
    m.insert(
        Cow::Borrowed(b"ut_metadata"),
        Value::Int(i64::from(ut_metadata_id)),
    );
    let mut dict = BTreeMap::<Cow<'_, [u8]>, Value<'_>>::new();
    dict.insert(Cow::Borrowed(b"m"), Value::Dict(m));
    encode(&Value::Dict(dict))
}

async fn wait_for_extension_handshake<S>(
    framed: &mut Framed<S, WireCodec>,
) -> u8
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("never received extension handshake from seeder");
        }
        let frame = tokio::time::timeout(Duration::from_millis(500), framed.next()).await;
        match frame {
            Ok(Some(Ok(Message::Extended { id: 0, payload }))) => {
                // Parse the extension handshake to get ut_metadata id
                let dict =
                    magpie_bt_bencode::decode(&payload).expect("decode extension handshake");
                let d = dict.as_dict().expect("ext hs is dict");
                let m = d
                    .get(&b"m"[..])
                    .and_then(|v| v.as_dict())
                    .expect("ext hs has m dict");
                let ut_id = u8::try_from(
                    m.get(&b"ut_metadata"[..])
                        .and_then(magpie_bt_bencode::Value::as_int)
                        .expect("ext hs has ut_metadata"),
                )
                .expect("ut_metadata id fits in u8");
                return ut_id;
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            Ok(Some(Err(e))) => panic!("wire error: {e:?}"),
            Ok(None) => panic!("eof"),
        }
    }
}

