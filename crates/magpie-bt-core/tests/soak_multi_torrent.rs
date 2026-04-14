//! M2 weekly-soak workload: ≥8 magpie engine pairs running concurrently
//! over loopback, each transferring a synthetic torrent end-to-end. Runs
//! repeated cycles for `SOAK_DURATION_SECS` (default 60s for local; the
//! weekly cron sets it to ~24h).
//!
//! `#[ignore]`'d so it never runs in the default `cargo test` suite —
//! invoked explicitly by `ci/soak/multi-torrent.sh`.
//!
//! Asserts (per gate criterion 3):
//! - Every cycle of every pair completes with SHA-256 match (no silent
//!   data corruption under sustained load).
//! - No engine panics or hangs (timeout per cycle bounded).
//! - Optional: when `SOAK_LARGE_PIECE_COUNT >= 100000`, one of the pairs
//!   uses a large-piece-count torrent to exercise ADR-0005's linear
//!   picker cost model.
//!
//! What's deliberately deferred (file as separate follow-ups when wired):
//! - dhat heap profiling (needs binary or example with global allocator;
//!   see ci/soak/dhat.sh).
//! - RSS-budget assertion (needs documented budget + measurement path).
//! - Continuous-running mode (each cycle currently tears down + re-spins;
//!   a single long-lived engine pool is the proper end state).
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::manual_assert
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha256;
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

const SMALL_PIECE_LENGTH: u32 = 16 * 1024;
const SMALL_PIECE_COUNT: u32 = 32; // 512 KiB per pair, fast cycle.

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn build_params(piece_count: u32, piece_length: u32, piece_hashes: Vec<u8>) -> TorrentParams {
    TorrentParams {
        piece_count,
        piece_length: u64::from(piece_length),
        total_length: u64::from(piece_count) * u64::from(piece_length),
        piece_hashes,
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

/// One full cycle: spin up a seeder + leecher pair for the given torrent,
/// drive the leecher to completion, assert SHA-256 match, tear both down.
async fn run_pair(
    pair_id: u32,
    seed_id_byte: u8,
    leech_id_byte: u8,
    piece_length: u32,
    piece_count: u32,
    seed: u64,
    cycle_timeout: Duration,
) {
    let synth = synthetic_torrent_v1(
        &format!("soak-{pair_id}.bin"),
        piece_length,
        piece_count,
        seed,
    );
    let info_hash = synth.info_hash;
    let content_sha = sha256(&synth.content);
    let pieces = extract_pieces(&synth.torrent);
    let total = u64::from(piece_count) * u64::from(piece_length);

    // Seed engine.
    let seed_alerts = Arc::new(AlertQueue::new(256));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let seed_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(total));
    seed_storage
        .write_block(0, &synth.content)
        .expect("seed write");
    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        build_params(piece_count, piece_length, pieces.clone()),
        Arc::clone(&seed_storage),
        // 20 bytes total: "-Mg0001-soakseed" is 16 chars; pad with pair id + bytes
        {
            let mut id = *b"-Mg0001-soakseed0000";
            id[16] = seed_id_byte;
            id[17] = (pair_id & 0xFF) as u8;
            id
        },
    );
    seed_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    seed_req.initial_have = vec![true; piece_count as usize];
    let seed_tid = seed_engine.add_torrent(seed_req).await.expect("seed add");
    let seed_listen = ListenConfig {
        peer_filter: Arc::new(DefaultPeerFilter::permissive_for_tests()),
        ..ListenConfig::default()
    };
    let seed_addr = seed_engine
        .listen("127.0.0.1:0".parse().unwrap(), seed_listen)
        .await
        .expect("seed listen");

    // Leech engine.
    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let leech_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(total));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(piece_count, piece_length, pieces),
        Arc::clone(&leech_storage),
        {
            let mut id = *b"-Mg0001-soakleech000";
            id[17] = leech_id_byte;
            id[18] = (pair_id & 0xFF) as u8;
            id
        },
    );
    leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leech_tid = leech_engine
        .add_torrent(leech_req)
        .await
        .expect("leech add");
    leech_engine
        .add_peer(leech_tid, seed_addr)
        .await
        .expect("leech connect");

    // Drive leech to completion.
    let deadline = Instant::now() + cycle_timeout;
    let mut completed = 0_usize;
    while completed < piece_count as usize {
        if Instant::now() > deadline {
            panic!("soak pair {pair_id}: cycle timed out at {completed}/{piece_count} pieces");
        }
        let drained = leech_alerts.drain();
        completed += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        if completed < piece_count as usize {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Verify.
    let mut got = vec![0u8; total as usize];
    leech_storage.read_block(0, &mut got).expect("leech read");
    assert_eq!(
        sha256(&got),
        content_sha,
        "soak pair {pair_id} cycle: SHA-256 mismatch (silent corruption)"
    );

    seed_engine.shutdown(seed_tid).await;
    leech_engine.shutdown(leech_tid).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "soak — invoked by ci/soak/multi-torrent.sh"]
async fn multi_torrent_soak() {
    let duration_secs = env_secs("SOAK_DURATION_SECS", 60);
    let pairs = env_usize("SOAK_PAIRS", 8);
    let large_pieces = env_usize("SOAK_LARGE_PIECE_COUNT", 0);

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let cycle_timeout = Duration::from_secs(60);
    let mut cycle = 0_u64;

    while Instant::now() < deadline {
        cycle += 1;
        let mut handles = Vec::with_capacity(pairs);
        for pair_id in 0..pairs {
            let pid = pair_id as u32;
            // Per-pair seed varies with cycle so successive cycles
            // exercise different content (no piece-cache nirvana).
            let pair_seed = u64::from(pid) ^ cycle.wrapping_mul(0xC0FFEE);
            handles.push(tokio::spawn(run_pair(
                pid,
                0x10 + (pid & 0x3F) as u8,
                0x80 + (pid & 0x3F) as u8,
                SMALL_PIECE_LENGTH,
                SMALL_PIECE_COUNT,
                pair_seed,
                cycle_timeout,
            )));
        }
        // Optionally one large-piece-count pair per cycle to exercise the
        // linear picker (ADR-0005). Gated by env so local runs stay fast.
        if large_pieces >= 100_000 {
            let lp = u32::try_from(large_pieces).expect("large_pieces fits u32");
            handles.push(tokio::spawn(run_pair(
                u32::try_from(pairs).unwrap_or(u32::MAX),
                0xAA,
                0xBB,
                8 * 1024,
                lp,
                cycle.wrapping_mul(0xDEAD_BEEF),
                Duration::from_secs(600), // 100k pieces needs more headroom
            )));
        }
        for h in handles {
            h.await.expect("pair task panic");
        }
        eprintln!(
            "[soak] cycle {cycle} complete; elapsed {:?}",
            deadline.saturating_duration_since(Instant::now())
        );
    }
    eprintln!("[soak] finished after {cycle} cycle(s) in {duration_secs}s budget");
}
