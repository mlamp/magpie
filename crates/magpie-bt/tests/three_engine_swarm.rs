//! M4 hard-gate verification (milestone gate 2): three magpie engines
//! on loopback, each with its own DHT. A seeds a synthetic torrent;
//! B and C discover A's address **via DHT alone** (no trackers, no
//! PEX, no pre-configured peers) and download every byte.
//!
//! Silent-failure guard: the leech engines never call `add_peer`
//! from the test; the only route peer addresses can reach them is
//! through the `attach_dht` announce loop. A download completing
//! end-to-end is therefore positive proof the DHT → engine plumbing
//! is live.
//!
//! Requires both the `dht` and `test-support` features.

#![cfg(all(feature = "dht", feature = "test-support"))]
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt::dht::{DhtRuntimeConfig, NodeId};
use magpie_bt::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt::peer_filter::DefaultPeerFilter;
use magpie_bt::session::TorrentParams;
use magpie_bt::storage::{MemoryStorage, Storage};
use magpie_bt::{AttachDhtConfig, TorrentId};
use magpie_bt_core::dht::spawn_dht_on_demux;
use magpie_bt_core::session::udp::UdpDemux;
use magpie_bt_metainfo::sha256;
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 32;
const TOTAL: u64 = PIECE_LENGTH as u64 * PIECE_COUNT as u64;
const TIMEOUT: Duration = Duration::from_secs(45);
const DHT_ROUND_INTERVAL: Duration = Duration::from_millis(250);

#[allow(clippy::missing_const_for_fn)] // Vec<u8> + u64::from keep this non-const
fn build_params(piece_hashes: Vec<u8>) -> TorrentParams {
    TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: u64::from(PIECE_LENGTH),
        total_length: TOTAL,
        piece_hashes,
        private: false,
    }
}

fn extract_piece_hashes(torrent: &[u8]) -> Vec<u8> {
    magpie_bt_metainfo::parse(torrent)
        .expect("synthetic torrent parses")
        .info
        .v1
        .as_ref()
        .expect("v1 info present")
        .pieces
        .to_vec()
}

const fn node_id(byte: u8) -> NodeId {
    let mut bytes = [0u8; 20];
    bytes[0] = byte;
    // Non-zero tail avoids Kademlia edge cases where the sender id
    // equals the target id for find_node(local_id).
    bytes[19] = 1;
    NodeId::from_bytes(bytes)
}

const fn dht_cfg() -> AttachDhtConfig {
    AttachDhtConfig {
        listen_port: 0,
        private: false,
        announce_interval: DHT_ROUND_INTERVAL,
        error_backoff: Duration::from_millis(500),
    }
}

async fn build_leech(
    info_hash: [u8; 20],
    piece_hashes: Vec<u8>,
    peer_id: [u8; 20],
) -> (Arc<Engine>, Arc<AlertQueue>, Arc<dyn Storage>, TorrentId) {
    let alerts = Arc::new(AlertQueue::new(512));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut req = AddTorrentRequest::new(
        info_hash,
        build_params(piece_hashes),
        Arc::clone(&storage),
        peer_id,
    );
    req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let tid = engine.add_torrent(req).await.expect("leech add_torrent");
    (engine, alerts, storage, tid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)] // integration-test scenario; splitting obscures the linear story
async fn three_engine_dht_only_swarm_download() {
    // --- 1. Synthetic torrent (deterministic) ----------------------------
    let synth = synthetic_torrent_v1("three_engine_swarm.bin", PIECE_LENGTH, PIECE_COUNT, 0xD47);
    let info_hash = synth.info_hash;
    let content_sha = sha256(&synth.content);
    let piece_hashes = extract_piece_hashes(&synth.torrent);

    // --- 2. Three UdpDemuxes on loopback --------------------------------
    let (demux_a, _task_a) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind demux a");
    let (demux_b, _task_b) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind demux b");
    let (demux_c, _task_c) = UdpDemux::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind demux c");
    let addr_a = demux_a.local_addr().unwrap();
    let addr_b = demux_b.local_addr().unwrap();
    let addr_c = demux_c.local_addr().unwrap();

    // --- 3. Three DHT runtimes on those demuxes -------------------------
    let now = Instant::now();
    let dht_a = spawn_dht_on_demux(
        Arc::clone(&demux_a),
        DhtRuntimeConfig::new(node_id(0x11)),
        now,
    )
    .await
    .expect("spawn dht a");
    let dht_b = spawn_dht_on_demux(
        Arc::clone(&demux_b),
        DhtRuntimeConfig::new(node_id(0x22)),
        now,
    )
    .await
    .expect("spawn dht b");
    let dht_c = spawn_dht_on_demux(
        Arc::clone(&demux_c),
        DhtRuntimeConfig::new(node_id(0x33)),
        now,
    )
    .await
    .expect("spawn dht c");

    // Seed each routing table with the other two. Production would
    // resolve the ADR-0025 cache + DNS path; this is a hermetic
    // short-circuit so the gate test has no external dependencies.
    for (runtime, contacts) in [
        (
            dht_a.runtime.clone(),
            [(node_id(0x22), addr_b), (node_id(0x33), addr_c)],
        ),
        (
            dht_b.runtime.clone(),
            [(node_id(0x11), addr_a), (node_id(0x33), addr_c)],
        ),
        (
            dht_c.runtime.clone(),
            [(node_id(0x11), addr_a), (node_id(0x22), addr_b)],
        ),
    ] {
        for (id, addr) in contacts {
            runtime.seed_contact(id, addr, now).await;
        }
    }

    // --- 4. Seeder engine (A) -------------------------------------------
    let alerts_a = Arc::new(AlertQueue::new(512));
    alerts_a.set_mask(AlertCategory(u32::MAX));
    let engine_a = Arc::new(Engine::new(Arc::clone(&alerts_a)));

    let storage_a: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    storage_a
        .write_block(0, &synth.content)
        .expect("seed storage write");

    let mut req_a = AddTorrentRequest::new(
        info_hash,
        build_params(piece_hashes.clone()),
        Arc::clone(&storage_a),
        *b"-Mg0001-dht3eng01abc",
    );
    req_a.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    req_a.initial_have = vec![true; PIECE_COUNT as usize];
    let tid_a = engine_a.add_torrent(req_a).await.expect("A add_torrent");

    let listen_a = engine_a
        .listen(
            "127.0.0.1:0".parse().unwrap(),
            ListenConfig {
                peer_filter: Arc::new(DefaultPeerFilter::permissive_for_tests()),
                ..ListenConfig::default()
            },
        )
        .await
        .expect("A listen");

    engine_a
        .attach_dht(
            tid_a,
            dht_a.runtime.clone(),
            AttachDhtConfig {
                listen_port: listen_a.port(),
                ..dht_cfg()
            },
        )
        .await
        .expect("A attach_dht");

    // --- 5. Leecher engines (B and C) -----------------------------------
    let (engine_b, alerts_b, storage_b, tid_b) =
        build_leech(info_hash, piece_hashes.clone(), *b"-Mg0001-dht3engleecB").await;
    let (engine_c, alerts_c, storage_c, tid_c) =
        build_leech(info_hash, piece_hashes.clone(), *b"-Mg0001-dht3engleecC").await;

    engine_b
        .attach_dht(tid_b, dht_b.runtime.clone(), dht_cfg())
        .await
        .expect("B attach_dht");
    engine_c
        .attach_dht(tid_c, dht_c.runtime.clone(), dht_cfg())
        .await
        .expect("C attach_dht");

    // --- 6. Drive: wait for both leechers to complete -------------------
    let deadline = Instant::now() + TIMEOUT;
    let mut b_completed = 0_usize;
    let mut c_completed = 0_usize;
    let mut b_alerts: Vec<Alert> = Vec::new();
    let mut c_alerts: Vec<Alert> = Vec::new();
    while b_completed < PIECE_COUNT as usize || c_completed < PIECE_COUNT as usize {
        assert!(
            Instant::now() <= deadline,
            "download did not complete in {TIMEOUT:?}.\n\
             B pieces: {b_completed}/{PIECE_COUNT}; C pieces: {c_completed}/{PIECE_COUNT}\n\
             B alerts: {b_alerts:?}\n\
             C alerts: {c_alerts:?}"
        );
        let drained_b = alerts_b.drain();
        b_completed += drained_b
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        b_alerts.extend(drained_b);
        let drained_c = alerts_c.drain();
        c_completed += drained_c
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        c_alerts.extend(drained_c);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // --- 7. SHA-256 on both leech storages ------------------------------
    let mut got_b = vec![0u8; TOTAL as usize];
    storage_b.read_block(0, &mut got_b).expect("B read");
    assert_eq!(
        sha256(&got_b),
        content_sha,
        "B SHA-256 must match seed content"
    );

    let mut got_c = vec![0u8; TOTAL as usize];
    storage_c.read_block(0, &mut got_c).expect("C read");
    assert_eq!(
        sha256(&got_c),
        content_sha,
        "C SHA-256 must match seed content"
    );

    // --- 8. Silent-failure guards ---------------------------------------
    // (a) Both leechers saw a PeerConnected event — proves the DHT
    //     announce loop actually produced an add_peer call (the only
    //     add_peer source in this test). A completed download without
    //     PeerConnected would imply a plumbing short-circuit.
    assert!(
        b_alerts
            .iter()
            .any(|a| matches!(a, Alert::PeerConnected { .. })),
        "B never saw PeerConnected — add_peer never fired, download was spurious"
    );
    assert!(
        c_alerts
            .iter()
            .any(|a| matches!(a, Alert::PeerConnected { .. })),
        "C never saw PeerConnected — add_peer never fired, download was spurious"
    );
    // (b) B and C did not emit any DhtAnnounceFailed — if the DHT
    //     path errored, the swarm would still attempt direct tracker
    //     discovery (attach_tracker) as a fallback. Make sure we're
    //     testing the DHT path end-to-end, not a recovery mode.
    let had_dht_error = |alerts: &[Alert]| {
        alerts.iter().any(|a| {
            matches!(
                a,
                Alert::Error {
                    code: magpie_bt::alerts::AlertErrorCode::DhtAnnounceFailed,
                    ..
                }
            )
        })
    };
    assert!(
        !had_dht_error(&b_alerts),
        "B saw DhtAnnounceFailed — gate fails open; alerts: {b_alerts:?}"
    );
    assert!(
        !had_dht_error(&c_alerts),
        "C saw DhtAnnounceFailed — gate fails open; alerts: {c_alerts:?}"
    );

    // --- 9. Tear down ---------------------------------------------------
    engine_a.shutdown(tid_a).await;
    engine_b.shutdown(tid_b).await;
    engine_c.shutdown(tid_c).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), engine_a.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), engine_b.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), engine_c.join()).await;
}
