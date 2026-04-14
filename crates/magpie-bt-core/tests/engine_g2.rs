//! G2 (consumer-surface audit gap): `Engine::remove(id, delete_files)`.
//!
//! Covers:
//! - remove(false) is equivalent to shutdown — torrent gone from registry,
//!   on-disk file remains;
//! - `remove(true)` unlinks the `FileStorage`'s path;
//! - unknown id returns `TorrentNotFoundError`;
//! - in-memory storage delete is a no-op (default `Storage::delete`).
//!
//! Path safety is documented at the trait level (magpie does not derive
//! paths from torrent metainfo). No path-traversal test is necessary on
//! this surface — the consumer's path passes through unchanged.
#![cfg(unix)]
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;

use magpie_bt_core::alerts::AlertQueue;
use magpie_bt_core::engine::{AddTorrentRequest, Engine, TorrentId, TorrentNotFoundError};
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{FileStorage, MemoryStorage, Storage};
use tempfile::tempdir;

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

#[tokio::test]
async fn remove_keeps_files_when_delete_files_false() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("torrent.dat");
    let storage: Arc<dyn Storage> = Arc::new(FileStorage::create(&path, TOTAL).unwrap());
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine
        .add_torrent(AddTorrentRequest::new(
            [0xAB; 20],
            params(),
            storage,
            *b"-MP0001-0123456789ab",
        ))
        .await
        .unwrap();

    engine.remove(id, false).await.expect("remove ok");
    assert!(
        engine.torrents().await.is_empty(),
        "id removed from registry"
    );
    assert!(
        path.exists(),
        "file must still be on disk when delete_files=false"
    );

    engine.join().await;
}

#[tokio::test]
async fn remove_unlinks_file_when_delete_files_true() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("torrent.dat");
    let storage: Arc<dyn Storage> = Arc::new(FileStorage::create(&path, TOTAL).unwrap());
    assert!(path.exists());
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine
        .add_torrent(AddTorrentRequest::new(
            [0xCD; 20],
            params(),
            storage,
            *b"-MP0001-0123456789ab",
        ))
        .await
        .unwrap();

    engine.remove(id, true).await.expect("remove ok");
    assert!(engine.torrents().await.is_empty());
    assert!(
        !path.exists(),
        "file must be unlinked when delete_files=true"
    );

    engine.join().await;
}

#[tokio::test]
async fn remove_unknown_returns_not_found() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let bogus = TorrentId::__test_new(u64::MAX);
    let err = engine.remove(bogus, true).await.expect_err("must reject");
    let TorrentNotFoundError(reported) = err;
    assert_eq!(reported, bogus);
}

#[tokio::test]
async fn remove_with_memory_storage_is_noop_delete() {
    // Default `Storage::delete` returns Ok(()); MemoryStorage relies on it.
    // Test that remove(true) succeeds and registry is cleared without error.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine
        .add_torrent(AddTorrentRequest::new(
            [0xEF; 20],
            params(),
            storage,
            *b"-MP0001-0123456789ab",
        ))
        .await
        .unwrap();

    engine
        .remove(id, true)
        .await
        .expect("memory delete is no-op");
    assert!(engine.torrents().await.is_empty());
    engine.join().await;
}
