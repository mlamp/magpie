//! M2 gate criterion 2b: throughput floor. Confirms that when a peer's
//! shaper bucket is pinned at rate R, observed upload throughput ≥ 0.80 × R
//! (the gate's "≥ 80% of pinned rate" bar).
//!
//! **Methodology**: loopback has effectively unbounded bandwidth, so we
//! pin the rate via the shaper and measure vs the pinned rate — not vs
//! the NIC. This is the honest read of "80% of link rate" for an
//! in-process test (per the M2 plan red-team).
//!
//! Setup: magpie-seed (pre-populated, initial_have=all true) →
//! magpie-leech over `127.0.0.1:0`. Seed's peer bucket is rate-pinned
//! after `register_peer` replaces the default passthrough. Drive for a
//! fixed wall-clock window, then compute `bytes_transferred /
//! elapsed_secs` and assert ≥ 0.80 × pinned_rate.
#![cfg(unix)]
#![allow(missing_docs, clippy::cast_possible_truncation, clippy::cast_precision_loss,
    clippy::cast_sign_loss, clippy::doc_markdown, clippy::manual_assert,
    clippy::await_holding_lock, clippy::identity_op, clippy::uninlined_format_args,
    clippy::unchecked_time_subtraction)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

/// Pinned rate in bytes per second. 1 MiB/s is well under loopback
/// capacity (so the shaper is the bottleneck, not the wire) and above
/// tokio's scheduling jitter floor (so the measurement is stable).
const PINNED_RATE_BPS: u64 = 1 * 1024 * 1024;

const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 256; // 4 MiB total — ~4s at 1 MiB/s.
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
    magpie_bt_metainfo::parse(torrent)
        .expect("parses")
        .info
        .v1
        .as_ref()
        .expect("v1")
        .pieces
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shaper_pinned_rate_observed_within_tolerance() {
    // --- seed --------------------------------------------------------
    let synth = synthetic_torrent_v1("throughput.bin", PIECE_LENGTH, PIECE_COUNT, 0xBEEF);
    let info_hash = synth.info_hash;
    let pieces = extract_pieces(&synth.torrent);
    let seed_alerts = Arc::new(AlertQueue::new(256));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let seed_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    seed_storage.write_block(0, &synth.content).unwrap();
    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces.clone()),
        Arc::clone(&seed_storage),
        *b"-Mg0001-tflseed01abc",
    );
    seed_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    seed_req.initial_have = vec![true; PIECE_COUNT as usize];
    let seed_tid = seed_engine.add_torrent(seed_req).await.expect("seed add");
    let seed_listen = ListenConfig {
        peer_filter: Arc::new(DefaultPeerFilter::permissive_for_tests()),
        ..ListenConfig::default()
    };
    let seed_addr = seed_engine
        .listen("127.0.0.1:0".parse().unwrap(), seed_listen)
        .await
        .expect("seed listen");

    // --- leech -------------------------------------------------------
    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let leech_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces),
        Arc::clone(&leech_storage),
        *b"-Mg0001-tflleech01ab",
    );
    leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leech_tid = leech_engine.add_torrent(leech_req).await.expect("leech add");
    leech_engine
        .add_peer(leech_tid, seed_addr)
        .await
        .expect("connect");

    // --- pin the seed's uploader bucket at PINNED_RATE_BPS -----------
    //
    // The seed's Engine registered its peer in the shaper on add_peer
    // (from our side). We need the peer on the SEED side — the inbound
    // accept path spawns the peer task there. Wait briefly for the seed
    // to see the incoming connection and register its peer slot, then
    // rewrite the rate on the up bucket.
    //
    // The shaper handle is engine-private. For the test we access via a
    // short poll loop that touches any of the seed's registered peers'
    // up bucket. Expose via the engine's public `shaper()` accessor (to
    // be added if not present — see below).
    let shaper = seed_engine.shaper();
    let pin_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if Instant::now() > pin_deadline {
            panic!("seed never registered its inbound peer in the shaper");
        }
        let peers = shaper.peers.lock().unwrap();
        if let Some((_, pb)) = peers.iter().next() {
            pb.buckets.up.set_rate_bps(PINNED_RATE_BPS);
            // Also drain the bucket so the rate pin takes effect immediately
            // (the default cap starts full; without a drain the first 4 MiB
            // of tokens are "free" and the measurement would over-report).
            let cap = pb.buckets.up.capacity();
            let _ = pb.buckets.up.try_consume(cap);
            break;
        }
        drop(peers);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // --- drive + measure --------------------------------------------
    let start = Instant::now();
    let drive_timeout = Duration::from_secs(20);
    let mut completed = 0_usize;
    while completed < PIECE_COUNT as usize {
        if start.elapsed() > drive_timeout {
            panic!(
                "throughput test timed out at {completed}/{PIECE_COUNT} pieces"
            );
        }
        let drained = leech_alerts.drain();
        completed += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let elapsed = start.elapsed();
    let observed_bps = (TOTAL as f64) / elapsed.as_secs_f64();
    let floor = 0.80 * (PINNED_RATE_BPS as f64);

    eprintln!(
        "[throughput_floor] pinned={} B/s observed={:.0} B/s elapsed={:?} floor={}",
        PINNED_RATE_BPS, observed_bps, elapsed, floor as u64
    );

    assert!(
        observed_bps >= floor,
        "observed throughput {observed_bps:.0} B/s must be >= 80% of pinned {PINNED_RATE_BPS} B/s (floor {:.0} B/s)",
        floor
    );

    // Ceiling check: allow up to 2× pinned rate to catch a shaper that
    // silently passes through. The default cap (4 MiB) can grant a
    // one-time burst > pinned rate; 2× is comfortable headroom without
    // masking a bypass bug.
    let ceiling = 2.0 * (PINNED_RATE_BPS as f64);
    assert!(
        observed_bps <= ceiling,
        "observed throughput {observed_bps:.0} B/s exceeds 2× pinned rate {PINNED_RATE_BPS} B/s — shaper may be bypassed"
    );

    seed_engine.shutdown(seed_tid).await;
    leech_engine.shutdown(leech_tid).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
}
