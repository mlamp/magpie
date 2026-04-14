//! G1 (consumer-surface audit gap): `Engine::pause` / `Engine::resume`.
//!
//! Engine-level smoke tests that exercise the two new commands end-to-end:
//! - pause/resume on a known id returns `Ok(())`;
//! - both are idempotent (pausing twice is fine);
//! - unknown ids return `TorrentNotFoundError`;
//! - shutdown invalidates pause/resume (subsequent calls return `NotFound`).
//!
//! Deeper invariants (peer choke broadcast, scheduler gating) are unit-tested
//! at the actor level via direct `set_paused` calls — see
//! `crates/magpie-bt-core/src/session/torrent.rs` test module.
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;

use magpie_bt_core::alerts::AlertQueue;
use magpie_bt_core::engine::{AddTorrentRequest, Engine, TorrentId, TorrentNotFoundError};
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::MemoryStorage;

const PIECE_LENGTH: u64 = 32 * 1024;
const PIECE_COUNT: u32 = 2;
const TOTAL: u64 = PIECE_LENGTH * PIECE_COUNT as u64;

fn request(info_hash: [u8; 20]) -> AddTorrentRequest {
    let params = TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: PIECE_LENGTH,
        total_length: TOTAL,
        piece_hashes: vec![0u8; PIECE_COUNT as usize * 20],
        private: false,
    };
    AddTorrentRequest::new(
        info_hash,
        params,
        Arc::new(MemoryStorage::new(TOTAL)),
        *b"-MP0001-0123456789ab",
    )
}

#[tokio::test]
async fn pause_then_resume_succeeds() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine.add_torrent(request([0x11; 20])).await.unwrap();

    engine.pause(id).await.expect("pause known id");
    engine.resume(id).await.expect("resume known id");

    engine.shutdown(id).await;
    engine.join().await;
}

#[tokio::test]
async fn pause_is_idempotent() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine.add_torrent(request([0x22; 20])).await.unwrap();

    engine.pause(id).await.expect("first pause");
    engine.pause(id).await.expect("second pause is a no-op");
    engine.resume(id).await.expect("resume");
    engine.resume(id).await.expect("second resume is a no-op");

    engine.shutdown(id).await;
    engine.join().await;
}

#[tokio::test]
async fn pause_unknown_id_returns_not_found() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let bogus = TorrentId::__test_new(u64::MAX);
    let err = engine.pause(bogus).await.expect_err("must reject");
    let TorrentNotFoundError(reported) = err;
    assert_eq!(reported, bogus);
}

#[tokio::test]
async fn resume_unknown_id_returns_not_found() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let bogus = TorrentId::__test_new(u64::MAX);
    let err = engine.resume(bogus).await.expect_err("must reject");
    let TorrentNotFoundError(reported) = err;
    assert_eq!(reported, bogus);
}

#[tokio::test]
async fn pause_after_shutdown_returns_not_found() {
    let engine = Engine::new(Arc::new(AlertQueue::new(16)));
    let id = engine.add_torrent(request([0x33; 20])).await.unwrap();

    engine.shutdown(id).await;
    let err = engine
        .pause(id)
        .await
        .expect_err("shutdown removes torrent from registry");
    let TorrentNotFoundError(reported) = err;
    assert_eq!(reported, id);

    engine.join().await;
}
