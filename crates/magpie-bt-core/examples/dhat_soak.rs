//! dhat-instrumented soak workload for heap-allocation profiling.
//!
//! Runs the same seed-leech pair workload as `soak_multi_torrent.rs` but as
//! a standalone binary with a dhat global allocator. When the `Profiler` is
//! dropped at exit, dhat writes `dhat-heap.json` in the working directory.
//!
//! Additionally captures peak RSS via `getrusage(RUSAGE_SELF)` every 60 s
//! and writes `peak-rss.json` at exit.
//!
//! Env vars:
//!   SOAK_DURATION_SECS  — total runtime (default 300)
//!   SOAK_PAIRS          — concurrent seed-leech pairs (default 4)
//!
//! Build:
//!   cargo build --release -p magpie-bt-core --example dhat_soak --features dhat-heap
//!
//! Unix-only (libc::getrusage for RSS sampling).
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::unreadable_literal,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::borrow_as_ptr
)]

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
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

/// Sample peak RSS in KiB via `getrusage(RUSAGE_SELF)`.
///
/// Returns `ru_maxrss` in KiB. On macOS `ru_maxrss` is in bytes so we
/// convert; on Linux it is already in KiB.
fn peak_rss_kib() -> i64 {
    // SAFETY: `getrusage` writes into a zeroed `rusage` struct we own.
    // No invariants to uphold beyond passing a valid pointer.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let rss = usage.ru_maxrss;
        if cfg!(target_os = "macos") {
            rss / 1024 // macOS reports bytes
        } else {
            rss // Linux reports KiB
        }
    }
}

/// Background task that samples peak RSS every 60 s and stores the maximum.
async fn rss_sampler(peak: Arc<AtomicI64>, duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        let rss = peak_rss_kib();
        peak.fetch_max(rss, Ordering::Relaxed);
        eprintln!("[dhat-soak] RSS snapshot: {rss} KiB");
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let duration_secs = env_secs("SOAK_DURATION_SECS", 300);
    let pairs = env_usize("SOAK_PAIRS", 4);

    eprintln!("[dhat-soak] duration={duration_secs}s pairs={pairs}");

    let peak_rss = Arc::new(AtomicI64::new(0));
    let duration = Duration::from_secs(duration_secs);

    // Spawn RSS sampler.
    let rss_handle = {
        let peak = Arc::clone(&peak_rss);
        tokio::spawn(rss_sampler(peak, duration))
    };

    let deadline = Instant::now() + duration;
    let cycle_timeout = Duration::from_secs(60);
    let mut cycle = 0_u64;

    while Instant::now() < deadline {
        cycle += 1;
        let mut handles = Vec::with_capacity(pairs);
        for pair_id in 0..pairs {
            let pid = pair_id as u32;
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
        let mut failures = 0u32;
        for h in handles {
            match h.await {
                Ok(()) => {}
                Err(e) => {
                    failures += 1;
                    eprintln!("[dhat-soak] pair failed in cycle {cycle}: {e}");
                }
            }
        }
        eprintln!(
            "[dhat-soak] cycle {cycle} complete (failures: {failures}); remaining {:?}",
            deadline.saturating_duration_since(Instant::now())
        );
    }

    // Final RSS sample.
    let final_rss = peak_rss_kib();
    peak_rss.fetch_max(final_rss, Ordering::Relaxed);
    rss_handle.abort();

    let observed_peak = peak_rss.load(Ordering::Relaxed);
    eprintln!(
        "[dhat-soak] finished after {cycle} cycle(s) in {duration_secs}s; peak RSS {observed_peak} KiB"
    );

    // Write peak-rss.json.
    let rss_json = format!(
        "{{\n  \"peak_rss_kib\": {observed_peak},\n  \"cycles\": {cycle},\n  \"duration_secs\": {duration_secs},\n  \"pairs\": {pairs}\n}}\n"
    );
    std::fs::write("peak-rss.json", rss_json).expect("write peak-rss.json");
    eprintln!("[dhat-soak] wrote peak-rss.json");

    // dhat::Profiler is dropped here, writing dhat-heap.json.
}
