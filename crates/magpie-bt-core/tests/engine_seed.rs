//! Seed-side integration test.
//!
//! Spins up an `Engine` with a torrent whose pieces are pre-seeded via
//! `initial_have`, has a mock leecher client initiate over loopback, and
//! verifies that after `Interested + Request` the client receives the
//! correct block bytes.
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
use magpie_bt_core::alerts::{AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::{HandshakeRole, PeerConfig, TorrentParams, perform_handshake};
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha1;
use magpie_bt_wire::{BlockRequest, Message, WireCodec};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

const PIECE_LENGTH: u64 = 16 * 1024;
const PIECE_COUNT: u32 = 2;
const TOTAL: u64 = PIECE_LENGTH * PIECE_COUNT as u64;

fn make_payload() -> Vec<u8> {
    (0..TOTAL)
        .map(|i| (i as u8).wrapping_mul(11).wrapping_add(3))
        .collect()
}

fn piece_hashes(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 * PIECE_COUNT as usize);
    for piece in 0..PIECE_COUNT {
        let start = (piece as u64 * PIECE_LENGTH) as usize;
        let end = start + PIECE_LENGTH as usize;
        out.extend_from_slice(&sha1(&payload[start..end]));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_seeds_prepopulated_torrent_to_external_leecher() {
    let payload = make_payload();
    let hashes = piece_hashes(&payload);
    let info_hash = [0x42u8; 20];

    // Prepare storage with the full torrent bytes already in place.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    storage.write_block(0, &payload).unwrap();

    let alerts = Arc::new(AlertQueue::new(128));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let mut req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: PIECE_COUNT,
            piece_length: PIECE_LENGTH,
            total_length: TOTAL,
            piece_hashes: hashes,
            private: false,
        },
        Arc::clone(&storage),
        *b"-Mg0001-seedersesion",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.initial_have = vec![true; PIECE_COUNT as usize];
    let _tid = engine.add_torrent(req).await.expect("add_torrent");

    // Bind the inbound listener.
    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    // Mock leecher: connect, handshake, send Interested, request a block,
    // assert the reply matches.
    let mut stream = TcpStream::connect(bound).await.expect("connect");
    let cfg = PeerConfig {
        peer_id: *b"-Mg0001-mockleechrxx",
        info_hash,
        fast_ext: true,
        max_in_flight: 4,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_secs(5),
    };
    perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator)
        .await
        .expect("handshake");

    let mut framed = Framed::new(stream, WireCodec::new(256 * 1024));
    framed
        .send(Message::Interested)
        .await
        .expect("send interested");

    // Expect Unchoke from the seeder.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let got_unchoke;
    loop {
        if std::time::Instant::now() > deadline {
            panic!("never received Unchoke from seeder");
        }
        let frame = tokio::time::timeout(Duration::from_millis(500), framed.next()).await;
        match frame {
            Ok(Some(Ok(Message::Unchoke))) => {
                got_unchoke = true;
                break;
            }
            Ok(Some(Ok(_other))) => continue, // ignore Have / HaveAll / etc
            Ok(Some(Err(e))) => panic!("wire err: {e:?}"),
            Ok(None) => panic!("eof before unchoke"),
            Err(_) => continue,
        }
    }
    assert!(got_unchoke);

    // Request block 0 (full piece 0 in one read-size slice — test piece
    // length matches BLOCK_SIZE).
    let req_frame = BlockRequest::new(0, 0, PIECE_LENGTH as u32);
    framed
        .send(Message::Request(req_frame))
        .await
        .expect("send request");

    // Expect the matching Piece response.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let got_block;
    loop {
        if std::time::Instant::now() > deadline {
            panic!("never received block");
        }
        let frame = tokio::time::timeout(Duration::from_millis(500), framed.next()).await;
        match frame {
            Ok(Some(Ok(Message::Piece(block)))) => {
                assert_eq!(block.piece, 0);
                assert_eq!(block.offset, 0);
                let expected = &payload[0..PIECE_LENGTH as usize];
                assert_eq!(&block.data[..], expected, "seeded block bytes mismatch");
                got_block = true;
                break;
            }
            Ok(Some(Ok(_other))) => continue,
            Ok(Some(Err(e))) => panic!("wire err: {e:?}"),
            Ok(None) => panic!("eof before block"),
            Err(_) => continue,
        }
    }
    assert!(got_block);
}
