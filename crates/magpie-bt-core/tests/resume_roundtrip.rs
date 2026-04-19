//! M2 Track A — resume-state round-trip (ADR-0022).
//!
//! Two-leech scenario: seed engine hosts a fixture; first leech
//! downloads some pieces, is paused, and persists a resume sidecar,
//! then shuts down. A second leech loads the sidecar, is added with
//! `initial_have` prepopulated, and completes only the remaining
//! pieces. End SHA-256 match.
//!
//! **Why pause?** Loopback is too fast to reliably catch a specific
//! mid-download piece count by polling alerts alone — the leech goes
//! from 0 to all-pieces-complete in a single drain tick. Pausing
//! after the first `PieceCompleted` freezes the picker at whatever
//! partial state it reached; the test then verifies resume works for
//! that exact partial state, whatever it happens to be.
//!
//! Silent-failure guards:
//! - The persisted bitfield must represent a *partial* download
//!   (1..=PIECE_COUNT-1 pieces), otherwise the test would be vacuous.
//! - The resume leech must emit exactly `PIECE_COUNT - persisted`
//!   `PieceCompleted` alerts — if it emits `PIECE_COUNT`, the resume
//!   was ignored.
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;
use std::time::Duration;

use std::time::Instant;

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::resume::{FileResumeSink, ResumeSink, ResumeSnapshot};
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha256;
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

/// Seed-side up-rate while the first leech is running. Low enough that
/// pause-after-first-piece actually catches a partial state (otherwise
/// loopback delivers the whole 256 KB in a single tick).
const FIRST_LEECH_RATE_BPS: u64 = 16 * 1024; // ~1 piece/s

const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 16;
const TOTAL: u64 = PIECE_LENGTH as u64 * PIECE_COUNT as u64;

fn build_params(pieces: Vec<u8>) -> TorrentParams {
    TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: u64::from(PIECE_LENGTH),
        total_length: TOTAL,
        piece_hashes: pieces,
        private: false,
    }
}

fn extract_pieces(torrent: &[u8]) -> Vec<u8> {
    let parsed = magpie_bt_metainfo::parse(torrent).unwrap();
    parsed
        .info
        .v1
        .as_ref()
        .unwrap()
        .pieces
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn resume_from_sidecar_completes_remaining_pieces() {
    let synth = synthetic_torrent_v1("resume_fixture.bin", PIECE_LENGTH, PIECE_COUNT, 0xABCD);
    let info_hash = synth.info_hash;
    let content_sha = sha256(&synth.content);
    let pieces = extract_pieces(&synth.torrent);

    // ----- Seed -----
    let seed_alerts = Arc::new(AlertQueue::new(512));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let seed_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    seed_storage.write_block(0, &synth.content).unwrap();
    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces.clone()),
        Arc::clone(&seed_storage),
        *b"-Mg0001-resseed01abc",
    );
    seed_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    seed_req.initial_have = vec![true; PIECE_COUNT as usize];
    let seed_id = seed_engine.add_torrent(seed_req).await.unwrap();
    let seed_listen = ListenConfig {
        peer_filter: Arc::new(DefaultPeerFilter::permissive_for_tests()),
        ..ListenConfig::default()
    };
    let seed_addr = seed_engine
        .listen("127.0.0.1:0".parse().unwrap(), seed_listen)
        .await
        .unwrap();

    // ----- First leech: download until we see progress, pause, persist -----
    let sink_dir = tempfile::tempdir().unwrap();
    let sink = FileResumeSink::new(sink_dir.path()).unwrap();
    let leech1_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let partial_have = {
        let leech_alerts = Arc::new(AlertQueue::new(512));
        leech_alerts.set_mask(AlertCategory(u32::MAX));
        let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
        let mut leech_req = AddTorrentRequest::new(
            info_hash,
            build_params(pieces.clone()),
            Arc::clone(&leech1_storage),
            *b"-Mg0001-resleech1ab2",
        );
        leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
        let leech_id = leech_engine.add_torrent(leech_req).await.unwrap();
        leech_engine.add_peer(leech_id, seed_addr).await.unwrap();

        // Pin the seed's per-peer up-bucket to FIRST_LEECH_RATE_BPS so
        // pieces trickle instead of flooding in one tick.
        let seed_shaper = seed_engine.shaper();
        let pin_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                Instant::now() <= pin_deadline,
                "seed never registered its inbound peer in the shaper"
            );
            let pinned = {
                let peers = seed_shaper.peers.lock().unwrap();
                if let Some((_, pb)) = peers.iter().next() {
                    pb.buckets.up.set_rate_bps(FIRST_LEECH_RATE_BPS);
                    let cap = pb.buckets.up.capacity();
                    let _ = pb.buckets.up.try_consume(cap);
                    true
                } else {
                    false
                }
            };
            if pinned {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Wait for the first PieceCompleted, then pause immediately. The
        // pause stops new requests; whatever was already in flight may
        // complete, but the final partial state is frozen for us to
        // snapshot.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                Instant::now() <= deadline,
                "first leech never completed any piece"
            );
            let drained = leech_alerts.drain();
            let got_any = drained
                .iter()
                .any(|a| matches!(a, Alert::PieceCompleted { .. }));
            if got_any {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        leech_engine.pause(leech_id).await.expect("pause");
        // Allow any mid-flight pieces to settle before snapshotting.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let have = leech_engine
            .torrent_bitfield_snapshot(leech_id)
            .await
            .expect("bitfield snapshot");
        let completed_pieces = have.iter().filter(|b| **b).count();
        assert!(
            completed_pieces > 0 && completed_pieces < PIECE_COUNT as usize,
            "silent-failure guard: first leech must be partially complete \
             (got {completed_pieces} of {PIECE_COUNT})"
        );

        sink.enqueue(ResumeSnapshot {
            info_hash,
            have: have.clone(),
            piece_count: PIECE_COUNT,
            piece_length: u64::from(PIECE_LENGTH),
            total_length: TOTAL,
        })
        .unwrap();
        sink.flush_graceful(Duration::from_secs(5)).unwrap();

        leech_engine.shutdown(leech_id).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
        have
    };
    let partial_count = partial_have.iter().filter(|b| **b).count();

    // ----- Load sidecar, spin up a fresh engine, resume -----
    let loaded = sink
        .load_sidecar(&info_hash)
        .unwrap()
        .expect("sidecar must exist");
    assert_eq!(loaded.info_hash, info_hash);
    assert_eq!(loaded.piece_count, PIECE_COUNT);
    assert_eq!(loaded.piece_length, u64::from(PIECE_LENGTH));
    assert_eq!(loaded.total_length, TOTAL);
    assert_eq!(loaded.have.len(), PIECE_COUNT as usize);
    assert_eq!(
        loaded.have, partial_have,
        "loaded bitfield must bit-for-bit match what was persisted"
    );

    // Second leech reuses the SAME storage (the on-disk blocks are
    // still good from leech 1) and populates `initial_have` from the
    // sidecar — so it only has to fetch the remaining half.
    let leech2_alerts = Arc::new(AlertQueue::new(512));
    leech2_alerts.set_mask(AlertCategory(u32::MAX));
    let leech2_engine = Arc::new(Engine::new(Arc::clone(&leech2_alerts)));
    let mut leech2_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces.clone()),
        Arc::clone(&leech1_storage),
        *b"-Mg0001-resleech2ab2",
    );
    leech2_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    leech2_req.initial_have = loaded.have.clone();
    let leech2_id = leech2_engine.add_torrent(leech2_req).await.unwrap();
    leech2_engine.add_peer(leech2_id, seed_addr).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut completed_after_resume = 0usize;
    let expected_remaining = (PIECE_COUNT as usize) - partial_count;
    while completed_after_resume < expected_remaining {
        assert!(
            Instant::now() <= deadline,
            "resume leech didn't finish remaining {expected_remaining} pieces in 30s \
             (got {completed_after_resume})"
        );
        let drained = leech2_alerts.drain();
        completed_after_resume += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        if completed_after_resume < expected_remaining {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Final verification: storage SHA-256 matches seed content.
    let mut got = vec![0u8; TOTAL as usize];
    leech1_storage.read_block(0, &mut got).unwrap();
    assert_eq!(
        sha256(&got),
        content_sha,
        "resumed leech content SHA-256 must match seed"
    );

    // Silent-failure guard: the resume leech must have emitted EXACTLY
    // `expected_remaining` PieceCompleted alerts, not PIECE_COUNT —
    // otherwise it re-downloaded everything and initial_have was ignored.
    assert_eq!(
        completed_after_resume, expected_remaining,
        "resume leech should only complete the missing half; \
         if this equals PIECE_COUNT the resume was ignored"
    );

    seed_engine.shutdown(seed_id).await;
    leech2_engine.shutdown(leech2_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech2_engine.join()).await;
}
