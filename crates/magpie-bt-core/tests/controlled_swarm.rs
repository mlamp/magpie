//! M2 hard-gate verification (gate criterion 2): magpie-only controlled
//! swarm. One magpie [`Engine`] seeds a synthetic torrent over loopback;
//! a second magpie [`Engine`] leeches it; final SHA-256 of the leecher's
//! storage matches the seeder's content.
//!
//! Sizing: 1 MiB at 16 KiB pieces (64 pieces). Smaller than the milestone's
//! "~5 MiB" guidance to keep the in-process test snappy and reliable —
//! the gate's *correctness* signal (every byte matches under SHA-256) does
//! not require a particular size; the larger payload is reserved for the
//! soak harness and the third-party-leech variant of this scenario, both
//! of which are tracked separately.
//!
//! No third-party leech here — that's a best-effort companion (interop
//! harness) tracked in a separate stage.
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;
use std::time::Duration;

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha256;
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 64;
const TOTAL: u64 = PIECE_LENGTH as u64 * PIECE_COUNT as u64;

fn build_params(piece_hashes: Vec<u8>) -> TorrentParams {
    TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: u64::from(PIECE_LENGTH),
        total_length: TOTAL,
        piece_hashes,
        private: false,
    }
}

fn extract_piece_hashes_from_torrent(torrent: &[u8]) -> Vec<u8> {
    // synthetic_torrent_v1 produces a v1 torrent — parse and return the
    // pieces blob. Avoids round-tripping through SHA recomputation.
    let parsed = magpie_bt_metainfo::parse(torrent).expect("synthetic torrent parses");
    parsed
        .info
        .v1
        .as_ref()
        .expect("v1 info present")
        .pieces
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magpie_seed_to_magpie_leech_sha256_match() {
    // 1. Generate the synthetic torrent (deterministic via seed).
    let synth = synthetic_torrent_v1("controlled_swarm.bin", PIECE_LENGTH, PIECE_COUNT, 0xC0DE);
    let info_hash = synth.info_hash;
    let content_sha256 = sha256(&synth.content);
    let pieces = extract_piece_hashes_from_torrent(&synth.torrent);

    // 2. Stand up the seed Engine: pre-seeded MemoryStorage + initial_have
    //    all-true so the seed serves immediately.
    let seed_alerts = Arc::new(AlertQueue::new(256));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let seed_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    seed_storage
        .write_block(0, &synth.content)
        .expect("seed storage write");
    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces.clone()),
        Arc::clone(&seed_storage),
        *b"-Mg0001-cswseed01abc",
    );
    seed_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    seed_req.initial_have = vec![true; PIECE_COUNT as usize];
    let seed_id = seed_engine
        .add_torrent(seed_req)
        .await
        .expect("seed add_torrent");
    let seed_listen_cfg = ListenConfig {
        peer_filter: Arc::new(DefaultPeerFilter::permissive_for_tests()),
        ..ListenConfig::default()
    };
    let seed_addr = seed_engine
        .listen("127.0.0.1:0".parse().unwrap(), seed_listen_cfg)
        .await
        .expect("seed listen");

    // 3. Stand up the leech Engine: empty MemoryStorage, no initial_have.
    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let leech_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces),
        Arc::clone(&leech_storage),
        *b"-Mg0001-cswleech01ab",
    );
    leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leech_id = leech_engine
        .add_torrent(leech_req)
        .await
        .expect("leech add_torrent");
    leech_engine
        .add_peer(leech_id, seed_addr)
        .await
        .expect("leech add_peer");

    // 4. Drive: poll leech alerts until PIECE_COUNT PieceCompleted events.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut completed = 0_usize;
    let mut all_alerts: Vec<Alert> = Vec::new();
    while completed < PIECE_COUNT as usize {
        if std::time::Instant::now() > deadline {
            // Drain seed alerts too for diagnosis.
            let seed_drained = seed_alerts.drain();
            panic!(
                "controlled_swarm did not complete in 20s; got {completed}/{PIECE_COUNT} pieces.\n\
                 leech alerts seen: {all_alerts:?}\n\
                 seed alerts seen: {seed_drained:?}"
            );
        }
        let drained = leech_alerts.drain();
        completed += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        all_alerts.extend(drained);
        if completed < PIECE_COUNT as usize {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // 5. Pull the leecher's bytes out and SHA-256-verify against the
    //    seeder's content. This is the gate's hard signal: every byte of
    //    every piece must match end-to-end across the wire.
    let mut got = vec![0u8; TOTAL as usize];
    leech_storage.read_block(0, &mut got).expect("leech read");
    let leech_sha = sha256(&got);
    assert_eq!(
        leech_sha, content_sha256,
        "leech SHA-256 must match seed content SHA-256"
    );

    // Tear down.
    seed_engine.shutdown(seed_id).await;
    leech_engine.shutdown(leech_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
}
