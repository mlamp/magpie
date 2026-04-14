//! G3 (consumer-surface audit gap): `Engine::torrents()` + `torrent_state()`.
//!
//! Covers:
//! - fresh engine → empty list + `None` state;
//! - after `add_torrent` → id listed, state matches params;
//! - multiple torrents → all ids present (any order);
//! - unknown id → `None`;
//! - after `shutdown` → id no longer listed, state is `None`.
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;

use magpie_bt_core::alerts::AlertQueue;
use magpie_bt_core::engine::{AddTorrentRequest, Engine, TorrentStateView};
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::MemoryStorage;

const PIECE_LENGTH: u64 = 32 * 1024;
const PIECE_COUNT: u32 = 2;
const TOTAL: u64 = PIECE_LENGTH * PIECE_COUNT as u64;

fn params() -> TorrentParams {
    TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: PIECE_LENGTH,
        total_length: TOTAL,
        piece_hashes: vec![0u8; PIECE_COUNT as usize * 20],
        private: false,
    }
}

fn request(info_hash: [u8; 20]) -> AddTorrentRequest {
    AddTorrentRequest::new(
        info_hash,
        params(),
        Arc::new(MemoryStorage::new(TOTAL)),
        *b"-MP0001-0123456789ab",
    )
}

#[tokio::test]
async fn fresh_engine_reports_no_torrents() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    assert!(engine.torrents().await.is_empty());
}

#[tokio::test]
async fn state_after_add_matches_params() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let info_hash = [0x11u8; 20];
    let id = engine.add_torrent(request(info_hash)).await.unwrap();

    let listed = engine.torrents().await;
    assert_eq!(listed, vec![id]);

    let state: TorrentStateView = engine.torrent_state(id).await.expect("registered id");
    assert_eq!(state.info_hash, info_hash);
    assert_eq!(state.total_length, TOTAL);
    assert_eq!(state.peer_count, 0);
    assert!(state.peer_cap > 0, "default per-torrent cap must be positive");

    engine.shutdown(id).await;
    engine.join().await;
}

#[tokio::test]
async fn multiple_torrents_all_listed() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let a = engine.add_torrent(request([0xA1; 20])).await.unwrap();
    let b = engine.add_torrent(request([0xB2; 20])).await.unwrap();
    let c = engine.add_torrent(request([0xC3; 20])).await.unwrap();

    let mut listed = engine.torrents().await;
    listed.sort_by_key(|t| format!("{t:?}"));
    let mut expected = vec![a, b, c];
    expected.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(listed, expected);

    // Each id resolves; unknown ids return None.
    for id in &expected {
        let state = engine.torrent_state(*id).await.expect("registered");
        assert_eq!(state.total_length, TOTAL);
    }
    engine.shutdown(a).await;
    engine.shutdown(b).await;
    engine.shutdown(c).await;
    engine.join().await;
}

#[tokio::test]
async fn unknown_id_returns_none() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine.add_torrent(request([0xAA; 20])).await.unwrap();

    // Mint a bogus id the engine did not issue.
    let bogus = magpie_bt_core::engine::TorrentId::__test_new(u64::MAX);
    assert!(
        engine.torrent_state(bogus).await.is_none(),
        "bogus id must not resolve"
    );

    engine.shutdown(id).await;
    engine.join().await;
}

#[tokio::test]
async fn shutdown_removes_from_listing() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine.add_torrent(request([0xDD; 20])).await.unwrap();
    assert_eq!(engine.torrents().await.len(), 1);

    engine.shutdown(id).await;
    assert!(
        engine.torrents().await.is_empty(),
        "shutdown must remove from registry"
    );
    assert!(
        engine.torrent_state(id).await.is_none(),
        "shutdown must make torrent_state return None"
    );

    engine.join().await;
}
