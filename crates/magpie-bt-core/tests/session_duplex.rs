//! End-to-end leecher test using `tokio::io::duplex`.
//!
//! Spins up:
//! - a `TorrentSession` (the leecher) bound to a `MemoryStorage`,
//! - a hand-rolled in-process seeder running the wire codec directly,
//! - a single duplex pipe between them,
//!
//! and asserts the leecher fetches every piece, all hashes verify, and the
//! `AlertQueue` reports `PieceCompleted` for each piece plus
//! `TorrentState::Completed` at the end. No real sockets, no tokio runtime
//! needed by the seeder beyond `bytes`-shuffling.
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_collect,
    clippy::too_many_lines,
    clippy::manual_assert,
    clippy::unchecked_time_subtraction,
    clippy::significant_drop_tightening
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::session::{
    DEFAULT_DISK_QUEUE_CAPACITY, DiskWriter, HandshakeRole, PeerConfig, PeerConn, PeerSlot,
    TorrentParams, TorrentSession, perform_handshake,
};
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha1;
use magpie_bt_wire::{BLOCK_SIZE, Block, Message, WireCodec};
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

const PIECE_LENGTH: u64 = 32 * 1024; // two 16 KiB blocks per piece.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplex_leecher_fetches_synthetic_torrent() {
    let payload = make_payload();
    let hashes = piece_hashes(&payload);
    let info_hash = [0xCDu8; 20];
    let leech_peer_id = *b"-Mg0001-leecherabcde";
    let seed_peer_id = *b"-Mg0001-seederabcdef";

    // Set up the leecher.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let alerts = Arc::new(AlertQueue::new(64));
    alerts.set_mask(AlertCategory(u32::MAX));
    let params = TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: PIECE_LENGTH,
        total_length: TOTAL,
        piece_hashes: hashes.clone(),
        private: false,
    };
    let (peer_to_session_tx, peer_to_session_rx) =
        mpsc::channel(magpie_bt_core::session::PEER_TO_SESSION_CAPACITY);
    let (session_to_peer_tx, session_to_peer_rx) = mpsc::unbounded_channel();

    // Spawn the disk writer; session pushes verify+write ops onto its bounded queue.
    let (disk_writer, disk_tx, disk_metrics) =
        DiskWriter::new(Arc::clone(&storage), DEFAULT_DISK_QUEUE_CAPACITY);
    let disk_task = tokio::spawn(disk_writer.run());

    let read_cache = Arc::new(magpie_bt_core::session::read_cache::ReadCache::with_defaults());
    let (mut torrent, cmd_tx) = TorrentSession::new(
        magpie_bt_core::TorrentId::__test_new(1),
        params.clone(),
        info_hash,
        Arc::clone(&alerts),
        peer_to_session_rx,
        disk_tx.clone(),
        read_cache,
    );
    let slot = PeerSlot(1);
    assert!(torrent.register_peer(slot, session_to_peer_tx));

    // Wire up the duplex channel.
    let (leech_io, seed_io) = duplex(64 * 1024);

    // Spawn the leecher peer + torrent task.
    let peer_config = PeerConfig {
        peer_id: leech_peer_id,
        info_hash,
        fast_ext: true,
        max_in_flight: 4,
        max_payload: 256 * 1024,
        handshake_timeout: std::time::Duration::from_secs(5),
    };
    let leech_handshake_cfg = peer_config.clone();
    let leech_task = tokio::spawn(async move {
        let mut io = leech_io;
        let remote = perform_handshake(&mut io, &leech_handshake_cfg, HandshakeRole::Initiator)
            .await
            .expect("leecher handshake");
        let conn = PeerConn::new(
            io,
            slot,
            leech_handshake_cfg,
            peer_to_session_tx,
            session_to_peer_rx,
        );
        conn.run(remote).await;
    });
    let torrent_task = tokio::spawn(async move { torrent.run().await });

    // Drive the seeder inline.
    let seed_payload = payload.clone();
    let seed_task = tokio::spawn(async move {
        let mut io = seed_io;
        let cfg = PeerConfig {
            peer_id: seed_peer_id,
            info_hash,
            fast_ext: true,
            max_in_flight: 0,
            max_payload: 256 * 1024,
            handshake_timeout: std::time::Duration::from_secs(5),
        };
        let _remote = perform_handshake(&mut io, &cfg, HandshakeRole::Responder)
            .await
            .expect("seeder handshake");
        let mut framed = Framed::new(io, WireCodec::new(256 * 1024));
        // Send HaveAll (BEP 6) — we have every piece.
        framed.send(Message::HaveAll).await.unwrap();
        framed.send(Message::Unchoke).await.unwrap();
        // Service requests forever.
        while let Some(frame) = framed.next().await {
            match frame {
                Ok(Message::Request(req)) => {
                    let start = (req.piece as u64 * PIECE_LENGTH) as usize + req.offset as usize;
                    let end = start + req.length as usize;
                    let data = Bytes::copy_from_slice(&seed_payload[start..end]);
                    framed
                        .send(Message::Piece(Block::new(req.piece, req.offset, data)))
                        .await
                        .unwrap();
                }
                Ok(
                    Message::Interested
                    | Message::NotInterested
                    | Message::KeepAlive
                    | Message::Have(_),
                ) => {}
                Ok(other) => {
                    eprintln!("seeder ignoring {other:?}");
                }
                Err(e) => {
                    eprintln!("seeder framing error: {e}");
                    break;
                }
            }
        }
    });

    // Poll the alert ring for PIECE_COUNT `PieceCompleted` events, then send
    // Shutdown to let the session exit. (M2 session stays alive post-complete
    // to serve inbound requests; tests that want completion-return semantics
    // must shut down explicitly.)
    //
    // Collect alerts across polls — the later assertions re-check the sequence.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut all_alerts: Vec<Alert> = Vec::new();
    loop {
        all_alerts.extend(alerts.drain());
        let count = all_alerts
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        if count >= PIECE_COUNT as usize {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("did not see {PIECE_COUNT} PieceCompleted alerts within 5s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let _ = cmd_tx
        .send(magpie_bt_core::session::SessionCommand::Shutdown)
        .await;
    let final_state = tokio::time::timeout(std::time::Duration::from_secs(2), torrent_task)
        .await
        .expect("torrent should exit within 2s of Shutdown")
        .expect("torrent task panicked");
    assert_eq!(
        final_state,
        magpie_bt_core::session::TorrentState::Completed
    );

    // Storage byte-equality.
    let mut got = vec![0u8; TOTAL as usize];
    storage.read_block(0, &mut got).unwrap();
    assert_eq!(
        got, payload,
        "storage must hold the seeded payload byte-for-byte"
    );

    // Alert sequence should contain PieceCompleted * PIECE_COUNT.
    all_alerts.extend(alerts.drain());
    let completed: Vec<u32> = all_alerts
        .iter()
        .filter_map(|a| match a {
            Alert::PieceCompleted { piece, .. } => Some(*piece),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), PIECE_COUNT as usize, "got {all_alerts:?}");

    // ADR-0019: exactly one TorrentComplete alert must fire on the
    // leech→seed transition. Multiple fires would indicate a broken
    // completion_fired guard.
    let complete_count = all_alerts
        .iter()
        .filter(|a| matches!(a, Alert::TorrentComplete { .. }))
        .count();
    assert_eq!(
        complete_count, 1,
        "exactly one TorrentComplete alert expected on first transition; got {complete_count}",
    );

    // Disk metrics should reflect every verified+committed piece.
    assert_eq!(
        disk_metrics.pieces_written.load(Ordering::Relaxed),
        u64::from(PIECE_COUNT)
    );
    assert_eq!(disk_metrics.bytes_written.load(Ordering::Relaxed), TOTAL,);
    assert_eq!(disk_metrics.piece_verify_fail.load(Ordering::Relaxed), 0);
    assert_eq!(disk_metrics.io_failures.load(Ordering::Relaxed), 0);

    drop(leech_task);
    drop(seed_task);
    drop(disk_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), disk_task).await;
    let _ = BLOCK_SIZE; // ensure constant used
}
