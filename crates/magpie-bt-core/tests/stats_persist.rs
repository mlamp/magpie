//! M2 verification gate item 6: persistent stats survive a process restart.
//!
//! This is the **in-process variant**: simulates "process exit" by dropping
//! the `FileStatsSink` (so the in-memory `pending` queue is gone, exactly
//! as it would be on real exit) and reconstructing a new sink against the
//! same directory. Verifies the cumulative counters round-trip via the
//! bencode sidecar files.
//!
//! The plan also calls for a subprocess SIGKILL variant. That requires a
//! magpie binary, which doesn't exist yet (magpie-bt is a library). The
//! subprocess variant is filed as a follow-up; this in-process test
//! covers the persistence path's correctness in the meantime.
//!
//! Invariants asserted (per ADR-0014 / `feedback_plan_red_team`
//! "silent-failure fixtures"):
//! - pre-drop counters are **non-zero** (rules out the trivial-pass
//!   trap where both are 0 and round-trip "matches" vacuously);
//! - post-restart counters **>= pre-drop snapshot** (cumulative bound);
//! - the sidecar **is the actual file on disk** at the expected path
//!   (rules out a sink that silently keeps state in RAM only).
#![allow(missing_docs)]

use std::fmt::Write as _;
use std::path::Path;

use magpie_bt_core::session::stats::StatsSnapshot;
use magpie_bt_core::session::stats::sink::{FileStatsSink, StatsSink};
use tempfile::tempdir;

const fn snapshot(info_hash: [u8; 20], up: u64, down: u64) -> StatsSnapshot {
    StatsSnapshot {
        info_hash,
        uploaded: up,
        downloaded: down,
    }
}

fn sidecar_exists(dir: &Path, info_hash: &[u8; 20]) -> bool {
    let mut name = String::with_capacity(45);
    for b in info_hash {
        write!(&mut name, "{b:02x}").unwrap();
    }
    name.push_str(".stats");
    dir.join(name).exists()
}

#[test]
fn counters_survive_sink_drop_and_reconstruction() {
    let dir = tempdir().unwrap();
    let info_hash = [0xACu8; 20];

    // Phase 1: pre-restart sink. Enqueue non-zero counters and flush.
    let pre_up = 12_345_u64;
    let pre_down = 67_890_u64;
    {
        let sink = FileStatsSink::new(dir.path()).expect("create sink");
        sink.enqueue(snapshot(info_hash, pre_up, pre_down))
            .expect("enqueue");
        sink.flush_now().expect("flush_now");

        // Sidecar must be on disk before drop — otherwise we'd be testing
        // in-memory state, not persistence.
        assert!(
            sidecar_exists(dir.path(), &info_hash),
            "sidecar must be written to disk before drop"
        );
    }
    // `sink` is dropped here. Simulates the process exit: in-memory
    // `pending` queue is gone; only the disk artifact remains.

    // Phase 2: fresh sink, same directory. Must read counters back.
    let restored_sink = FileStatsSink::new(dir.path()).expect("re-create sink");
    let restored = restored_sink
        .load_sidecar(&info_hash)
        .expect("load_sidecar")
        .expect("sidecar present after restart");

    assert_eq!(restored.info_hash, info_hash);
    assert!(
        restored.uploaded > 0 && restored.downloaded > 0,
        "guards the silent-failure 0-counter trap; got up={} down={}",
        restored.uploaded,
        restored.downloaded
    );
    assert!(
        restored.uploaded >= pre_up,
        "post-restart uploaded must be >= pre-restart snapshot ({} vs {})",
        restored.uploaded,
        pre_up
    );
    assert!(
        restored.downloaded >= pre_down,
        "post-restart downloaded must be >= pre-restart snapshot ({} vs {})",
        restored.downloaded,
        pre_down
    );
    // Strict equality is also a fair gate here: the sink is single-writer
    // and we did one flush. Anything else is silent corruption.
    assert_eq!(restored.uploaded, pre_up);
    assert_eq!(restored.downloaded, pre_down);
}

#[test]
fn load_sidecar_returns_none_on_cold_start() {
    let dir = tempdir().unwrap();
    let sink = FileStatsSink::new(dir.path()).expect("create sink");
    let restored = sink.load_sidecar(&[0u8; 20]).expect("load on empty dir");
    assert!(restored.is_none(), "cold start must return None, not error");
}

#[test]
fn load_sidecar_rejects_truncated_file() {
    let dir = tempdir().unwrap();
    let sink = FileStatsSink::new(dir.path()).expect("create sink");
    let info_hash = [0xBEu8; 20];

    // Hand-write a malformed sidecar (truncated bencode dict).
    let path = sink.sidecar_path(&info_hash);
    std::fs::write(&path, b"d10:downloadedi67890e8:uploadedi1234").unwrap();

    let err = sink
        .load_sidecar(&info_hash)
        .expect_err("must fail on truncated input");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("decode") || msg.to_lowercase().contains("invalid"),
        "expected decode-class error, got: {msg}"
    );
}

#[test]
fn last_flush_wins_under_repeated_enqueue_then_restart() {
    // The dedup-on-info_hash semantics in `enqueue` mean only the latest
    // pre-flush value lands on disk. After restart, we must see that
    // last value, not an earlier one.
    let dir = tempdir().unwrap();
    let info_hash = [0xCDu8; 20];
    {
        let sink = FileStatsSink::new(dir.path()).expect("create sink");
        sink.enqueue(snapshot(info_hash, 10, 20)).unwrap();
        sink.enqueue(snapshot(info_hash, 99, 100)).unwrap();
        sink.flush_now().unwrap();
    }
    let sink2 = FileStatsSink::new(dir.path()).expect("re-create sink");
    let s = sink2.load_sidecar(&info_hash).unwrap().expect("present");
    assert_eq!(s.uploaded, 99);
    assert_eq!(s.downloaded, 100);
}
