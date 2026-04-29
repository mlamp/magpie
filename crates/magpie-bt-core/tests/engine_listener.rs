//! Inbound-TCP (A2) integration tests for `Engine::listen`.
//!
//! Scenarios:
//! - An external seeder initiates to our listener; engine reads handshake,
//!   routes by info_hash, replies, and drives the leech to completion.
//! - A connection whose info_hash is not registered is dropped without a
//!   handshake reply.
//! - Peer-ID collision on inbound is silent-dropped (plan invariant #6).
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::needless_collect,
    clippy::manual_assert,
    clippy::redundant_clone,
    clippy::used_underscore_binding,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::unused_async,
    clippy::field_reassign_with_default,
    clippy::significant_drop_tightening,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddPeerError, AddTorrentRequest, Engine, ListenConfig, PeerCapScope};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::{HandshakeRole, PeerConfig, TorrentParams, perform_handshake};
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha1;
use magpie_bt_wire::{Block, Message, WireCodec};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

const PIECE_LENGTH: u64 = 32 * 1024;
const PIECE_COUNT: u32 = 4;
const TOTAL: u64 = PIECE_LENGTH * PIECE_COUNT as u64;

fn make_payload() -> Vec<u8> {
    (0..TOTAL).map(|i| (i as u8).wrapping_mul(31)).collect()
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

async fn seeder_initiate(
    target: std::net::SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    payload: Arc<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = TcpStream::connect(target).await.expect("connect");
        let cfg = PeerConfig {
            peer_id,
            info_hash,
            fast_ext: true,
            extension_protocol: false,
            max_in_flight: 0,
            max_payload: 256 * 1024,
            handshake_timeout: Duration::from_secs(5),
            extension_handshake_timeout: Duration::from_secs(5),
            remote_addr: None,
            metadata_size: None,
            local_listen_port: None,
        };
        let _ = perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator)
            .await
            .expect("handshake");
        let mut framed = Framed::new(stream, WireCodec::new(256 * 1024));
        framed.send(Message::HaveAll).await.expect("have_all");
        framed.send(Message::Unchoke).await.expect("unchoke");
        while let Some(frame) = framed.next().await {
            match frame {
                Ok(Message::Request(req)) => {
                    let start = (req.piece as u64 * PIECE_LENGTH) as usize + req.offset as usize;
                    let end = start + req.length as usize;
                    let data = Bytes::copy_from_slice(&payload[start..end]);
                    if framed
                        .send(Message::Piece(Block::new(req.piece, req.offset, data)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_accepts_inbound_and_fetches_via_two_seeders() {
    let payload = Arc::new(make_payload());
    let hashes = piece_hashes(&payload);
    let info_hash = [0xAAu8; 20];

    let alerts = Arc::new(AlertQueue::new(128));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
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
        *b"-Mg0001-leecherabcde",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.handshake_timeout = Duration::from_secs(5);
    let torrent_id = engine.add_torrent(req).await.expect("add_torrent");

    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    // Two seeders both initiate to our listener.
    let h1 = seeder_initiate(
        bound,
        info_hash,
        *b"-Mg0001-inboundseed1",
        Arc::clone(&payload),
    )
    .await;
    let h2 = seeder_initiate(
        bound,
        info_hash,
        *b"-Mg0001-inboundseed2",
        Arc::clone(&payload),
    )
    .await;

    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    let mut total_completed = 0usize;
    let mut all_alerts_seen: Vec<String> = Vec::new();
    loop {
        let drained = alerts.drain();
        for a in &drained {
            all_alerts_seen.push(format!("{a:?}"));
            if matches!(a, Alert::PieceCompleted { .. }) {
                total_completed += 1;
            }
        }
        if total_completed >= PIECE_COUNT as usize {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "inbound-path did not complete in 60s; completed {total_completed}/{PIECE_COUNT}; \
                 all alerts seen ({}):\n{}",
                all_alerts_seen.len(),
                all_alerts_seen.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut got = vec![0u8; TOTAL as usize];
    storage.read_block(0, &mut got).unwrap();
    assert_eq!(
        got, *payload,
        "inbound-path storage must match seeded payload"
    );

    engine.shutdown(torrent_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), engine.join()).await;
    let _ = (h1, h2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_drops_unknown_info_hash_silently() {
    // Listener is up but no torrent registered → our handshake must not be
    // written. A connecting peer should observe EOF after sending its
    // handshake, not a reply.
    let alerts = Arc::new(AlertQueue::new(16));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    let mut stream = TcpStream::connect(bound).await.expect("connect");
    let cfg = PeerConfig {
        peer_id: *b"-Mg0001-initiatorxxx",
        info_hash: [0xFFu8; 20], // not registered
        fast_ext: true,
        extension_protocol: false,
        max_in_flight: 0,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_millis(500),
        extension_handshake_timeout: Duration::from_secs(5),
        remote_addr: None,
        metadata_size: None,
        local_listen_port: None,
    };
    let result = perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator).await;
    // Engine must have silently dropped the connection, so we read EOF when
    // expecting their handshake. Either Io or Timeout is acceptable here —
    // the key invariant is "no valid handshake came back".
    assert!(
        result.is_err(),
        "engine must not reply with a handshake for an unknown info_hash; got {result:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_silent_drops_peer_id_collision() {
    // Two inbound connections claim the same peer_id. First wins; second is
    // silent-dropped (no handshake reply) per plan invariant #6.
    //
    // Uses a "quiet" peer that handshakes then holds the connection open
    // without sending HaveAll/Unchoke — otherwise the leecher completes on
    // one seeder and tears down the session before the collision test fires.
    let _payload = Arc::new(make_payload());
    let hashes = piece_hashes(&_payload);
    let info_hash = [0xBBu8; 20];

    let alerts = Arc::new(AlertQueue::new(128));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
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
        *b"-Mg0001-leecherabcde",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.handshake_timeout = Duration::from_secs(5);
    let _torrent_id = engine.add_torrent(req).await.expect("add_torrent");

    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    let peer_id = *b"-Mg0001-duplicatpeer";
    // Quiet peer: handshake, then hold the socket open to keep peer_id
    // reserved inside the engine.
    let (tx_stop, rx_stop) = tokio::sync::oneshot::channel::<()>();
    let h1 = tokio::spawn(async move {
        let mut stream = TcpStream::connect(bound).await.expect("connect1");
        let cfg = PeerConfig {
            peer_id,
            info_hash,
            fast_ext: true,
            extension_protocol: false,
            max_in_flight: 0,
            max_payload: 256 * 1024,
            handshake_timeout: Duration::from_secs(5),
            extension_handshake_timeout: Duration::from_secs(5),
            remote_addr: None,
            metadata_size: None,
            local_listen_port: None,
        };
        perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator)
            .await
            .expect("handshake1");
        // Hold until told to close.
        let _ = rx_stop.await;
        drop(stream);
    });

    // Wait for engine to register peer 1 by watching for PeerConnected.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let drained = alerts.drain();
        if drained
            .iter()
            .any(|a| matches!(a, Alert::PeerConnected { .. }))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("peer 1 never registered");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Second connection with the *same* peer_id must fail to handshake
    // (silent drop — engine reads our handshake but never writes its reply).
    let mut stream = TcpStream::connect(bound).await.expect("connect2");
    let cfg = PeerConfig {
        peer_id,
        info_hash,
        fast_ext: true,
        extension_protocol: false,
        max_in_flight: 0,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_millis(500),
        extension_handshake_timeout: Duration::from_secs(5),
        remote_addr: None,
        metadata_size: None,
        local_listen_port: None,
    };
    let result = perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator).await;
    assert!(
        result.is_err(),
        "collision attempt must not receive a handshake reply; got {result:?}",
    );

    let _ = tx_stop.send(());
    let _ = h1.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_torrent_peer_cap_rejects_outbound() {
    // peer_cap=1: second outbound add_peer must fail with
    // AddPeerError::PeerCapExceeded { scope: Torrent }.
    let payload = Arc::new(make_payload());
    let hashes = piece_hashes(&payload);
    let info_hash = [0xC1u8; 20];

    let alerts = Arc::new(AlertQueue::new(128));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
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
        *b"-Mg0001-leecherabcde",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.peer_cap = 1;
    let tid = engine.add_torrent(req).await.expect("add_torrent");

    // Two seeders, but the engine may only attach one.
    let seed_a = spawn_quiet_seeder(info_hash, *b"-Mg0001-capseederxx1").await;
    let seed_b = spawn_quiet_seeder(info_hash, *b"-Mg0001-capseederxx2").await;

    engine.add_peer(tid, seed_a).await.expect("first add_peer");
    // Give the first peer a moment to fully register.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = engine.add_peer(tid, seed_b).await.unwrap_err();
    assert!(
        matches!(
            err,
            AddPeerError::PeerCapExceeded {
                scope: PeerCapScope::Torrent
            }
        ),
        "expected torrent cap exceeded; got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_peer_cap_rejects_outbound() {
    // Two torrents, global cap = 1. First torrent uses its slot; second
    // torrent's first peer must fail with scope=Global.
    let info_hash_a = [0xD1u8; 20];
    let info_hash_b = [0xD2u8; 20];
    let alerts = Arc::new(AlertQueue::new(128));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)).with_global_peer_cap(1));

    let storage_a: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let storage_b: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));

    let hashes = piece_hashes(&make_payload());
    let build_req = |info_hash: [u8; 20], storage: Arc<dyn Storage>| {
        let mut r = AddTorrentRequest::new(
            info_hash,
            TorrentParams {
                piece_count: PIECE_COUNT,
                piece_length: PIECE_LENGTH,
                total_length: TOTAL,
                piece_hashes: hashes.clone(),
                private: false,
            },
            storage,
            *b"-Mg0001-leecherabcde",
        );
        r.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
        r
    };

    let tid_a = engine
        .add_torrent(build_req(info_hash_a, Arc::clone(&storage_a)))
        .await
        .unwrap();
    let tid_b = engine
        .add_torrent(build_req(info_hash_b, Arc::clone(&storage_b)))
        .await
        .unwrap();

    let seed_a = spawn_quiet_seeder(info_hash_a, *b"-Mg0001-globalcap001").await;
    let seed_b = spawn_quiet_seeder(info_hash_b, *b"-Mg0001-globalcap002").await;

    engine
        .add_peer(tid_a, seed_a)
        .await
        .expect("first add_peer");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = engine.add_peer(tid_b, seed_b).await.unwrap_err();
    assert!(
        matches!(
            err,
            AddPeerError::PeerCapExceeded {
                scope: PeerCapScope::Global
            }
        ),
        "expected global cap exceeded; got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_silent_drops_when_cap_exceeded() {
    // Torrent peer_cap=1; one inbound connection fills the slot, the second
    // must be silent-dropped (no handshake reply) per plan A2 §caps.
    let info_hash = [0xE1u8; 20];
    let alerts = Arc::new(AlertQueue::new(128));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: PIECE_COUNT,
            piece_length: PIECE_LENGTH,
            total_length: TOTAL,
            piece_hashes: piece_hashes(&make_payload()),
            private: false,
        },
        storage,
        *b"-Mg0001-leecherabcde",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.peer_cap = 1;
    let _tid = engine.add_torrent(req).await.expect("add_torrent");

    let mut listen_cfg = ListenConfig::default();
    listen_cfg.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let bound = engine
        .listen("127.0.0.1:0".parse().unwrap(), listen_cfg)
        .await
        .expect("listen");

    // First inbound peer takes the slot via a quiet holder (handshake only).
    let (tx_stop, rx_stop) = tokio::sync::oneshot::channel::<()>();
    let h1 = tokio::spawn(async move {
        let mut stream = TcpStream::connect(bound).await.expect("connect1");
        let cfg = PeerConfig {
            peer_id: *b"-Mg0001-incapfiller1",
            info_hash,
            fast_ext: true,
            extension_protocol: false,
            max_in_flight: 0,
            max_payload: 256 * 1024,
            handshake_timeout: Duration::from_secs(5),
            extension_handshake_timeout: Duration::from_secs(5),
            remote_addr: None,
            metadata_size: None,
            local_listen_port: None,
        };
        perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator)
            .await
            .expect("handshake1");
        let _ = rx_stop.await;
        drop(stream);
    });

    // Wait for peer 1 to register.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let drained = alerts.drain();
        if drained
            .iter()
            .any(|a| matches!(a, Alert::PeerConnected { .. }))
        {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "peer 1 never registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Peer 2 attempts handshake; engine must silent-drop before replying.
    let mut stream = TcpStream::connect(bound).await.expect("connect2");
    let cfg = PeerConfig {
        peer_id: *b"-Mg0001-incapfiller2",
        info_hash,
        fast_ext: true,
        extension_protocol: false,
        max_in_flight: 0,
        max_payload: 256 * 1024,
        handshake_timeout: Duration::from_millis(500),
        extension_handshake_timeout: Duration::from_secs(5),
        remote_addr: None,
        metadata_size: None,
        local_listen_port: None,
    };
    let result = perform_handshake(&mut stream, &cfg, HandshakeRole::Initiator).await;
    assert!(
        result.is_err(),
        "cap-exceeded inbound must not receive a handshake reply; got {result:?}",
    );

    let _ = tx_stop.send(());
    let _ = h1.await;
}

/// A "quiet" seeder that handshakes (as Responder) then holds the connection
/// without sending any data. Used to keep peer slots reserved across test
/// assertions without triggering session completion.
async fn spawn_quiet_seeder(info_hash: [u8; 20], peer_id: [u8; 20]) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let cfg = PeerConfig {
                peer_id,
                info_hash,
                fast_ext: true,
                extension_protocol: false,
                max_in_flight: 0,
                max_payload: 256 * 1024,
                handshake_timeout: Duration::from_secs(5),
                extension_handshake_timeout: Duration::from_secs(5),
                remote_addr: None,
                metadata_size: None,
                local_listen_port: None,
            };
            let _ = perform_handshake(&mut stream, &cfg, HandshakeRole::Responder).await;
            // Hold forever — test does not depend on close.
            let mut buf = [0u8; 4096];
            loop {
                use tokio::io::AsyncReadExt;
                if stream.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
            }
        }
    });
    addr
}
