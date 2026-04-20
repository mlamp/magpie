//! End-to-end Engine test over real TCP loopback sockets.
//!
//! Spins up:
//! - 3 in-process synthetic seeders, each bound to `127.0.0.1:0`,
//! - one [`Engine`] running a leecher with `DefaultPeerFilter::permissive_for_tests`,
//! - exercises `Engine::add_torrent` + `Engine::add_peer` for every seeder.
//!
//! Asserts the leecher fetches every piece, all bytes match, and disk
//! metrics report the right counts.
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
    clippy::manual_let_else
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, AttachTrackerConfig, Engine};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::{HandshakeRole, PeerConfig, TorrentParams, perform_handshake};
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_core::tracker::{AnnounceFuture, AnnounceRequest, AnnounceResponse, Tracker};
use magpie_bt_metainfo::sha1;
use magpie_bt_wire::{Block, Message, WireCodec};
use tokio::net::TcpListener;
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

async fn spawn_seeder(
    payload: Arc<Vec<u8>>,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return,
        };
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
        let mut stream = stream;
        let _remote = match perform_handshake(&mut stream, &cfg, HandshakeRole::Responder).await {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut framed = Framed::new(stream, WireCodec::new(256 * 1024));
        if framed.send(Message::HaveAll).await.is_err() {
            return;
        }
        if framed.send(Message::Unchoke).await.is_err() {
            return;
        }
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
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_fetches_synthetic_torrent_from_three_tcp_seeders() {
    let payload = Arc::new(make_payload());
    let hashes = piece_hashes(&payload);
    let info_hash = [0xCDu8; 20];

    // Three seeders.
    let mut seeder_addrs = Vec::new();
    for i in 0..3u8 {
        let mut peer_id = [0u8; 20];
        peer_id[..8].copy_from_slice(b"-Mg0001-");
        peer_id[8] = b'S';
        peer_id[9] = b'e' + i;
        seeder_addrs.push(spawn_seeder(Arc::clone(&payload), info_hash, peer_id).await);
    }

    // Engine + alerts.
    let alerts = Arc::new(AlertQueue::new(128));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let leech_peer_id = *b"-Mg0001-leecherabcde";
    let mut req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: PIECE_COUNT,
            piece_length: PIECE_LENGTH,
            total_length: TOTAL,
            piece_hashes: hashes.clone(),
            private: false,
        },
        Arc::clone(&storage),
        leech_peer_id,
    );
    // Loopback peers — flip the filter to test mode.
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.handshake_timeout = Duration::from_secs(5);

    let torrent_id = engine.add_torrent(req).await.expect("valid torrent params");

    // Attach all three seeders.
    for addr in &seeder_addrs {
        engine.add_peer(torrent_id, *addr).await.expect("add_peer");
    }

    // Wait for completion alert (or timeout). 60 s budget because this
    // test spins 3 mock seeders + 1 engine leech on one runtime; under
    // coverage instrumentation + `--all-features` on shared CI runners
    // the budget must be very generous to avoid flakes.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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
                "did not complete within 60s; completed {total_completed}/{PIECE_COUNT}; \
                 all alerts seen ({}):\n{}",
                all_alerts_seen.len(),
                all_alerts_seen.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Storage byte-equality.
    let mut got = vec![0u8; TOTAL as usize];
    storage.read_block(0, &mut got).unwrap();
    assert_eq!(
        got, *payload,
        "storage must match seeded payload byte-for-byte"
    );

    // Disk metrics.
    let metrics = engine.disk_metrics(torrent_id).await.unwrap();
    assert_eq!(
        metrics.pieces_written.load(Ordering::Relaxed),
        u64::from(PIECE_COUNT)
    );
    assert_eq!(metrics.bytes_written.load(Ordering::Relaxed), TOTAL);
    assert_eq!(metrics.piece_verify_fail.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.io_failures.load(Ordering::Relaxed), 0);

    engine.shutdown(torrent_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), engine.join()).await;
}

/// Mock tracker that returns a fixed peer list on every announce.
struct MockTracker {
    peers: Vec<std::net::SocketAddr>,
}

impl Tracker for MockTracker {
    fn announce<'a>(&'a self, _req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
        let peers = self.peers.clone();
        Box::pin(async move {
            Ok(AnnounceResponse {
                interval: Duration::from_secs(2),
                min_interval: None,
                peers,
                tracker_id: None,
                complete: Some(1),
                incomplete: Some(0),
                warning: None,
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_attach_tracker_drives_announce_loop_and_filters_peers() {
    let payload = Arc::new(make_payload());
    let hashes = piece_hashes(&payload);
    let info_hash = [0xCDu8; 20];

    // Two seeders + one bogus loopback the filter would reject in default mode
    // (we use permissive mode here so the seeders work; the test instead
    // verifies the *attach_tracker* loop calls add_peer for every advertised
    // address).
    let seed_a = spawn_seeder(Arc::clone(&payload), info_hash, *b"-Mg0001-mockseederaa").await;
    let seed_b = spawn_seeder(Arc::clone(&payload), info_hash, *b"-Mg0001-mockseederbb").await;

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
            piece_hashes: hashes.clone(),
            private: false,
        },
        Arc::clone(&storage),
        *b"-Mg0001-leecherabcde",
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req.handshake_timeout = Duration::from_secs(5);
    let torrent_id = engine.add_torrent(req).await.unwrap();

    let tracker: Arc<dyn Tracker> = Arc::new(MockTracker {
        peers: vec![seed_a, seed_b],
    });
    engine
        .attach_tracker(torrent_id, tracker, AttachTrackerConfig::default())
        .await
        .unwrap();

    // 60 s budget — generous for shared CI runners under coverage.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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
                "attach_tracker did not drive completion in 60s; completed {total_completed}/{PIECE_COUNT}; \
                 all alerts seen ({}):\n{}",
                all_alerts_seen.len(),
                all_alerts_seen.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut got = vec![0u8; TOTAL as usize];
    storage.read_block(0, &mut got).unwrap();
    assert_eq!(got, *payload);

    engine.shutdown(torrent_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_rejects_invalid_torrent_params() {
    let alerts = Arc::new(AlertQueue::new(8));
    let engine = Engine::new(alerts);
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(1024));
    let req = AddTorrentRequest::new(
        [0u8; 20],
        TorrentParams {
            piece_count: 4,
            piece_length: 256,
            total_length: 1024,
            piece_hashes: vec![0u8; 20],
            private: false, // wrong length: should be 80
        },
        storage,
        [0u8; 20],
    );
    let err = engine.add_torrent(req).await.unwrap_err();
    assert!(matches!(
        err,
        magpie_bt_core::engine::AddTorrentError::InvalidParams(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_rejects_filtered_peer_address() {
    // Default filter rejects loopback; add_peer must surface AddPeerError::Filtered.
    let alerts = Arc::new(AlertQueue::new(16));
    let engine = Engine::new(alerts);
    let info_hash = [0u8; 20];
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(1024));
    let req = AddTorrentRequest::new(
        info_hash,
        TorrentParams {
            piece_count: 1,
            piece_length: 1024,
            total_length: 1024,
            piece_hashes: vec![0u8; 20],
            private: false,
        },
        storage,
        [0u8; 20],
    );
    let id = engine.add_torrent(req).await.unwrap();
    let err = engine
        .add_peer(id, "127.0.0.1:1".parse().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        magpie_bt_core::engine::AddPeerError::Filtered(_)
    ));
    engine.shutdown(id).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), engine.join()).await;
}
