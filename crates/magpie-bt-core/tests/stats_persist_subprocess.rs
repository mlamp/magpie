//! M2 gate criterion 6 — subprocess variant. Proves that seeder-side
//! upload counters survive a SIGKILL (not a graceful SIGINT) and are
//! restored by `FileStatsSink::load_sidecar` on the next run.
//!
//! Complements the in-process `stats_persist.rs` test. That one covers
//! the sink's drop-and-reconstruct behaviour without ever running a
//! real process. This one exercises the full chain: real binary, real
//! upload activity, real SIGKILL, real restart — the scenario a
//! production operator or CI incident actually hits.
//!
//! Per `feedback_plan_red_team` silent-failure-fixture rule: the test
//! asserts `uploaded > 0` **before** killing (so a zero-flush can't
//! vacuously match a zero-restore), and asserts the restored value is
//! `>=` the pre-kill snapshot AND `> 0` after restart.
#![cfg(unix)]
#![allow(missing_docs, clippy::cast_possible_truncation, clippy::too_many_lines,
    clippy::collapsible_if, clippy::items_after_statements, clippy::doc_markdown,
    clippy::single_match_else, clippy::option_if_let_else,
    clippy::manual_let_else)]

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt_core::alerts::{AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::stats::sink::FileStatsSink;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{MemoryStorage, Storage};
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

fn seeder_exe() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("target/debug/examples/seeder")
}

struct SpawnedSeeder {
    child: std::process::Child,
    port: u16,
}

fn spawn_seeder(
    torrent: &std::path::Path,
    data: &std::path::Path,
    stats_dir: &std::path::Path,
    flush_secs: u64,
) -> SpawnedSeeder {
    let mut child = Command::new(seeder_exe())
        .arg("--torrent").arg(torrent)
        .arg("--data").arg(data)
        .arg("--listen").arg("127.0.0.1:0")
        .arg("--stats-dir").arg(stats_dir)
        .arg("--allow-loopback")
        .arg("--flush-secs").arg(flush_secs.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn seeder");

    let stderr = child.stderr.take().expect("stderr");
    let stdout = child.stdout.take().expect("stdout");
    let (port_tx, port_rx) = channel::<u16>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut sent = false;
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("[seeder stderr] {line}");
            if !sent {
                if let Some(idx) = line.find("listening on 127.0.0.1:") {
                    let tail = &line[idx + "listening on 127.0.0.1:".len()..];
                    if let Ok(p) = tail.trim().parse::<u16>() {
                        let _ = port_tx.send(p);
                        sent = true;
                    }
                }
            }
        }
    });
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

    SpawnedSeeder { child, port }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns the seeder binary + issues SIGKILL; invoke explicitly"]
async fn seeder_upload_counters_survive_sigkill() {
    // Ensure the binary is up-to-date.
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "magpie-bt", "--example", "seeder"])
        .status()
        .expect("cargo build seeder");
    assert!(status.success(), "build failed");

    // Fixture: synthetic torrent + data file on tempdir.
    let synth = synthetic_torrent_v1("stats_subproc.bin", PIECE_LENGTH, PIECE_COUNT, 0xAACE);
    let info_hash = synth.info_hash;
    let pieces = extract_pieces(&synth.torrent);

    let dir = tempdir().unwrap();
    let torrent_path = dir.path().join("fixture.torrent");
    let data_path = dir.path().join("fixture.bin");
    let stats_path = dir.path().join("stats.d");
    std::fs::write(&torrent_path, &synth.torrent).unwrap();
    let mut f = std::fs::File::create(&data_path).unwrap();
    f.write_all(&synth.content).unwrap();
    f.sync_all().unwrap();

    // --- Phase 1: spawn seeder, drive leech to completion, confirm the
    // sidecar shows non-zero uploaded counters, then SIGKILL.
    let spawn = spawn_seeder(&torrent_path, &data_path, &stats_path, 1);
    let mut child = spawn.child;
    let port = spawn.port;

    // Leech in-process drives upload on seeder's side.
    let seed_addr = format!("127.0.0.1:{port}").parse().expect("addr parse");
    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let leech_storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(TOTAL));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(pieces),
        Arc::clone(&leech_storage),
        *b"-Mg0001-spkleech01ab",
    );
    leech_req.peer_filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
    let leech_tid = leech_engine.add_torrent(leech_req).await.expect("leech add");
    leech_engine.add_peer(leech_tid, seed_addr).await.expect("connect");

    // Wait for leech to complete and for the sink to flush at least one
    // sidecar with non-zero uploaded counters.
    let deadline = Instant::now() + Duration::from_secs(15);
    let pre_kill_uploaded: u64;
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("never observed non-zero uploaded sidecar before deadline");
        }
        // Drain leech alerts for progress visibility.
        let _drained = leech_alerts.drain();
        // Probe the sidecar.
        let probe_sink = FileStatsSink::new(&stats_path).expect("probe sink");
        if let Some(snap) = probe_sink.load_sidecar(&info_hash).expect("load sidecar") {
            if snap.uploaded > 0 {
                pre_kill_uploaded = snap.uploaded;
                eprintln!(
                    "pre-kill sidecar: uploaded={} downloaded={}",
                    snap.uploaded, snap.downloaded
                );
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        pre_kill_uploaded > 0,
        "silent-failure-fixture guard: pre-kill uploaded must be > 0"
    );

    // Shut down the leech cleanly before we SIGKILL the seeder.
    leech_engine.shutdown(leech_tid).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;

    // SIGKILL the seeder. Child::kill on Unix sends SIGKILL — an uncatchable
    // signal, so flush_graceful never runs. We rely on a prior 1s periodic
    // flush having landed the sidecar on disk.
    child.kill().expect("SIGKILL seeder");
    let _ = child.wait();
    eprintln!("seeder SIGKILLed");

    // --- Phase 2: restart the seeder pointing at the same --stats-dir.
    // The seeder's "stats: restored from ... uploaded N down M" startup
    // log line is our signal.
    let mut child2 = Command::new(seeder_exe())
        .arg("--torrent").arg(&torrent_path)
        .arg("--data").arg(&data_path)
        .arg("--listen").arg("127.0.0.1:0")
        .arg("--stats-dir").arg(&stats_path)
        .arg("--allow-loopback")
        .arg("--flush-secs").arg("1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("respawn seeder");

    let stderr = child2.stderr.take().expect("stderr");
    let stdout = child2.stdout.take().expect("stdout");
    let (restore_tx, restore_rx) = channel::<(u64, u64)>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("[seeder2 stderr] {line}");
            if let Some(idx) = line.find("stats: restored from ") {
                // Expected shape: "stats: restored from <path> — uploaded N down M"
                let rest = &line[idx..];
                let up = rest.split("uploaded ").nth(1).and_then(|s| s.split_whitespace().next());
                let dn = rest.split("down ").nth(1).and_then(|s| s.split_whitespace().next());
                if let (Some(u), Some(d)) = (up, dn) {
                    if let (Ok(u), Ok(d)) = (u.parse::<u64>(), d.parse::<u64>()) {
                        let _ = restore_tx.send((u, d));
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[seeder2 stdout] {line}");
        }
    });

    let (restored_up, restored_down) =
        match restore_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(v) => v,
            Err(_) => {
                let _ = child2.kill();
                panic!("seeder2 did not print 'stats: restored' within 5s");
            }
        };
    eprintln!("restored: uploaded={restored_up} downloaded={restored_down}");

    assert!(
        restored_up > 0,
        "restored uploaded must be > 0 (silent-failure-fixture guard)"
    );
    assert!(
        restored_up >= pre_kill_uploaded,
        "restored uploaded {restored_up} must be >= pre-kill {pre_kill_uploaded}"
    );

    // Tear down seeder2.
    let _ = child2.kill();
    let _ = child2.wait();
}
