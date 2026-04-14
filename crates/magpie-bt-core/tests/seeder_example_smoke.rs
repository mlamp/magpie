//! Smoke test for `magpie-bt/examples/seeder.rs`.
//!
//! Generates a synthetic torrent + matching data file, spawns the
//! seeder binary as a subprocess, then runs a magpie-leech in-process
//! that connects to it and verifies SHA-256 match.
//!
//! This is the precursor to task #23 (subprocess SIGKILL stats_persist)
//! — proves the example binary is well-formed end-to-end before the
//! `SIGKILL` test piles more requirements onto it.
//! SIGKILL test piles more requirements onto it.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::collapsible_if
)]

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
use magpie_bt_metainfo::sha256;
use magpie_bt_metainfo::test_support::synthetic_torrent_v1;
use tempfile::tempdir;

const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 32;
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
#[ignore = "spawns the built seeder binary; invoke explicitly via cargo test seeder_example_smoke"]
async fn magpie_leech_can_fetch_from_seeder_example_binary() {
    // --- build the example binary so we know it's there. Prefer release
    //     if built for release tests; debug is fine for the smoke.
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "magpie-bt", "--example", "seeder"])
        .status()
        .expect("cargo build seeder example");
    assert!(status.success(), "build failed");

    // --- synthetic torrent + data file ---
    let synth = synthetic_torrent_v1("seeder_smoke.bin", PIECE_LENGTH, PIECE_COUNT, 0xBEEF);
    let info_hash = synth.info_hash;
    let pieces = extract_pieces(&synth.torrent);
    let content_sha = sha256(&synth.content);

    let dir = tempdir().unwrap();
    let torrent_path = dir.path().join("smoke.torrent");
    let data_path = dir.path().join("smoke.bin");
    let stats_path = dir.path().join("stats.d");

    std::fs::write(&torrent_path, &synth.torrent).unwrap();
    // Data file = the exact content the torrent describes.
    let mut f = std::fs::File::create(&data_path).unwrap();
    f.write_all(&synth.content).unwrap();
    f.sync_all().unwrap();

    // --- spawn seeder on an ephemeral port ---
    // Locate the example binary. `CARGO_MANIFEST_DIR` of this crate is
    // `<root>/crates/magpie-bt-core`; the target dir is `<root>/target`.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let exe = workspace_root.join("target/debug/examples/seeder");
    assert!(exe.exists(), "seeder binary not found at {}", exe.display());

    let mut child = Command::new(&exe)
        .arg("--torrent")
        .arg(&torrent_path)
        .arg("--data")
        .arg(&data_path)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--stats-dir")
        .arg(&stats_path)
        .arg("--allow-loopback")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn seeder");

    // Read stderr on a background thread — continues draining after we
    // spot the "listening on" line so the seeder's subsequent eprintln!
    // calls don't SIGPIPE when the pipe closes. Same for stdout.
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let stderr = child.stderr.take().expect("stderr");
    let stdout = child.stdout.take().expect("stdout");
    let (port_tx, port_rx) = channel::<u16>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut port_sent = false;
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("[seeder stderr] {line}");
            if !port_sent {
                if let Some(idx) = line.find("listening on 127.0.0.1:") {
                    let tail = &line[idx + "listening on 127.0.0.1:".len()..];
                    if let Ok(p) = tail.trim().parse::<u16>() {
                        let _ = port_tx.send(p);
                        port_sent = true;
                    }
                }
            }
        }
    });
    // Drain stdout so its pipe buffer doesn't fill.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[seeder stdout] {line}");
        }
    });

    let port = match port_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(p) => p,
        Err(RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            panic!("seeder never printed listening-on line within 10s");
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            panic!("seeder stderr closed before listening-on line");
        }
    };

    // --- magpie leech (in-process) connects to the seeder ---
    let seed_addr = format!("127.0.0.1:{port}").parse().expect("addr parse");
    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let leech_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces),
        Arc::clone(&leech_storage),
        *b"-Mg0001-ssmkleech01a",
    );
    leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leech_tid = leech_engine
        .add_torrent(leech_req)
        .await
        .expect("leech add");
    leech_engine
        .add_peer(leech_tid, seed_addr)
        .await
        .expect("connect");

    let drive_deadline = Instant::now() + Duration::from_secs(15);
    let mut completed = 0_usize;
    while completed < PIECE_COUNT as usize {
        if Instant::now() > drive_deadline {
            let _ = child.kill();
            panic!("leech did not complete within 15s ({completed}/{PIECE_COUNT})");
        }
        let drained = leech_alerts.drain();
        completed += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut got = vec![0u8; TOTAL as usize];
    leech_storage.read_block(0, &mut got).expect("leech read");
    assert_eq!(
        sha256(&got),
        content_sha,
        "SHA-256 mismatch between seeder binary output and leech-side content"
    );

    leech_engine.shutdown(leech_tid).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;

    // Tear down seeder. SIGTERM first; SIGKILL if it doesn't exit.
    let _ = child.kill();
    let _ = child.wait();
}
