//! Gate #2 verification: N random-block round-trip on both `Storage` impls.
#![allow(missing_docs, clippy::cast_possible_truncation)]

#[cfg(unix)]
use magpie_bt_core::storage::FileStorage;
use magpie_bt_core::storage::{MemoryStorage, Storage};

/// Produce `count` non-overlapping blocks of `block_len` bytes each, with
/// deterministic pseudo-random payloads.
fn deterministic_blocks(count: usize, block_len: usize) -> Vec<(u64, Vec<u8>)> {
    let mut state: u64 = 0xdead_beef_cafe_babe;
    let mut next_byte = || {
        state = state
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(0x1337);
        (state >> 24) as u8
    };
    (0..count)
        .map(|i| {
            let offset = (i * block_len) as u64;
            let payload: Vec<u8> = (0..block_len).map(|_| next_byte()).collect();
            (offset, payload)
        })
        .collect()
}

fn run_roundtrip<S: Storage>(s: &S) {
    let blocks = deterministic_blocks(64, 128);
    for (offset, payload) in &blocks {
        s.write_block(*offset, payload).unwrap();
    }
    for (offset, expected) in &blocks {
        let mut buf = vec![0_u8; expected.len()];
        s.read_block(*offset, &mut buf).unwrap();
        assert_eq!(&buf, expected, "block at offset {offset} mismatched");
    }
}

#[test]
fn memory_storage_roundtrip() {
    let s = MemoryStorage::new(64 * 128); // exactly 64 non-overlapping 128-byte blocks
    run_roundtrip(&s);
}

#[cfg(unix)]
#[test]
fn file_storage_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let s = FileStorage::create(dir.path().join("t.dat"), 64 * 128).unwrap();
    run_roundtrip(&s);
}

/// Concurrent non-overlapping writes from multiple threads must all land
/// correctly. `FileExt::write_at` is thread-safe positional on Unix; this
/// verifies our wrapper preserves that.
#[cfg(unix)]
#[test]
fn file_storage_concurrent_non_overlapping() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(FileStorage::create(dir.path().join("t.dat"), 4 * 1024 * 16).unwrap());
    let blocks = deterministic_blocks(64, 1024);
    let per_thread = 16;
    let mut handles = Vec::new();
    for t in 0..4 {
        let s = Arc::clone(&s);
        let chunk: Vec<(u64, Vec<u8>)> = blocks
            .iter()
            .skip(t * per_thread)
            .take(per_thread)
            .cloned()
            .collect();
        handles.push(thread::spawn(move || {
            for (offset, payload) in &chunk {
                s.write_block(*offset, payload).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // Verify every block matches what we wrote.
    for (offset, expected) in &blocks {
        let mut buf = vec![0_u8; expected.len()];
        s.read_block(*offset, &mut buf).unwrap();
        assert_eq!(
            &buf, expected,
            "concurrent write corrupted block at {offset}"
        );
    }
}

#[cfg(unix)]
#[test]
fn memory_and_file_produce_identical_buffers() {
    let mem = MemoryStorage::new(16 * 64);
    let dir = tempfile::tempdir().unwrap();
    let file = FileStorage::create(dir.path().join("t.dat"), 16 * 64).unwrap();
    let blocks = deterministic_blocks(16, 64);
    for (offset, payload) in &blocks {
        mem.write_block(*offset, payload).unwrap();
        file.write_block(*offset, payload).unwrap();
    }
    for (offset, _) in &blocks {
        let mut m_buf = [0_u8; 64];
        let mut f_buf = [0_u8; 64];
        mem.read_block(*offset, &mut m_buf).unwrap();
        file.read_block(*offset, &mut f_buf).unwrap();
        assert_eq!(m_buf, f_buf, "mem/file diverged at offset {offset}");
    }
}
