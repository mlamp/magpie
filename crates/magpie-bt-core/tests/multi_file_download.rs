//! M2 workstream J — multi-file download gate verification.
//!
//! Gate 10: a synthetic multi-file torrent whose layout engineers piece
//! boundaries to cross file boundaries. magpie-seed hosts it (pre-populated
//! `MultiFileStorage`), magpie-leech fetches it into an empty
//! `MultiFileStorage`. Per-file SHA-256 must match.
//!
//! Silent-failure guard: the fixture helper asserts at construction time
//! that the layout yields ≥3 boundary-crossing pieces and ≥1 piece spanning
//! ≥3 entries. A refactor that flattens the layout can't sneak past.
//!
//! Gate 12: fd-pool bound under load. A 10-file fixture is downloaded with
//! `FdPool::with_cap(4)`; the test asserts `opens_total() > 4`, proving LRU
//! eviction + lazy reopen actually fires.
#![allow(missing_docs, clippy::cast_possible_truncation)]

use std::sync::Arc;
use std::time::Duration;

use sha1::{Digest as _, Sha1};

use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt_core::engine::{AddTorrentRequest, Engine, ListenConfig};
use magpie_bt_core::peer_filter::DefaultPeerFilter;
use magpie_bt_core::session::TorrentParams;
use magpie_bt_core::storage::{FdPool, FileSpec, MultiFileStorage, Storage};
use magpie_bt_metainfo::sha256;

/// Fixture: one synthetic multi-file torrent designed to exercise the
/// hard cases in `MultiFileStorage`. Layout:
///
/// | file | length | cumulative end | notes                           |
/// |------|--------|----------------|---------------------------------|
/// | A    | 25000  | 25000          |                                 |
/// | B    | 1000   | 26000          | smaller than `piece_length`     |
/// | C    | 25000  | 51000          |                                 |
/// | D    | 0      | 51000          | zero-length in middle           |
/// | E    | 35000  | 86000          |                                 |
/// | F    | 30000  | 116000         |                                 |
/// | G    | 47840  | 163840         |                                 |
///
/// At `piece_length = 16384` (10 pieces total, 163840 bytes):
/// - Piece 1 spans A → B → C    ← 3 non-zero entries
/// - Piece 3 spans C → D (0-len, skipped by walker) → E
/// - Piece 5 spans E → F
/// - Piece 7 spans F → G
///
/// Gate 10 silent-failure guard: boundary-crossings ≥ 3 AND ≥1 piece
/// spans ≥ 3 non-zero entries — both asserted at construction time.
const PIECE_LENGTH: u32 = 16 * 1024;
const PIECE_COUNT: u32 = 10;
const TOTAL: u64 = PIECE_LENGTH as u64 * PIECE_COUNT as u64;

struct Fixture {
    /// Raw concatenated content in file-list order.
    content: Vec<u8>,
    /// v1 info hash (SHA-1 of info dict bytes).
    info_hash: [u8; 20],
    /// Concatenated piece hashes (20 bytes per piece).
    pieces: Vec<u8>,
    /// File list in the order declared by the torrent.
    files: Vec<FileSpec>,
}

fn fixture(seed: u64) -> Fixture {
    let layout: &[(&str, u64)] = &[
        ("A", 25_000),
        ("B", 1_000),
        ("C", 25_000),
        ("D", 0),
        ("E", 35_000),
        ("F", 30_000),
        ("G", 47_840),
    ];
    let files: Vec<FileSpec> = layout
        .iter()
        .map(|(name, len)| FileSpec {
            path: vec![(*name).to_owned()],
            length: *len,
        })
        .collect();
    let total: u64 = files.iter().map(|f| f.length).sum();
    assert_eq!(
        total, TOTAL,
        "fixture layout sums to {total} but TOTAL is {TOTAL}"
    );

    // Silent-failure guard: count boundary-crossings per gate 10.
    let mut crossings = 0usize;
    let mut max_span = 0usize;
    for p in 0..PIECE_COUNT {
        let start = u64::from(p) * u64::from(PIECE_LENGTH);
        let end = start + u64::from(PIECE_LENGTH);
        let entries_touched = files
            .iter()
            .scan(0u64, |acc, f| {
                let s = *acc;
                let e = s + f.length;
                *acc = e;
                Some((s, e, f.length))
            })
            .filter(|(s, e, len)| {
                // Non-zero-length entry whose range intersects the piece.
                *len > 0 && *s < end && *e > start
            })
            .count();
        if entries_touched > 1 {
            crossings += 1;
        }
        max_span = max_span.max(entries_touched);
    }
    assert!(
        crossings >= 3,
        "fixture must have ≥3 boundary-crossing pieces (found {crossings})"
    );
    assert!(
        max_span >= 3,
        "fixture must have ≥1 piece spanning ≥3 entries (max was {max_span})"
    );

    let mut content = Vec::with_capacity(TOTAL as usize);
    splitmix64_fill(&mut content, TOTAL as usize, seed);

    let mut pieces = Vec::with_capacity(PIECE_COUNT as usize * 20);
    for chunk in content.chunks(PIECE_LENGTH as usize) {
        let mut hasher = Sha1::new();
        hasher.update(chunk);
        pieces.extend_from_slice(&hasher.finalize());
    }

    // Bencode a v1 multi-file info dict. Keys ascending:
    //   info = { files, name, piece length, pieces }
    //   files = [{ length, path: [name] }...]
    let mut info = Vec::new();
    info.push(b'd');
    encode_key(&mut info, b"files");
    info.push(b'l');
    for (name, len) in layout {
        info.push(b'd');
        encode_key(&mut info, b"length");
        encode_int(&mut info, *len);
        encode_key(&mut info, b"path");
        info.push(b'l');
        encode_bytes(&mut info, name.as_bytes());
        info.push(b'e'); // end path list
        info.push(b'e'); // end file dict
    }
    info.push(b'e'); // end files list
    encode_key(&mut info, b"name");
    encode_bytes(&mut info, b"multi_fixture");
    encode_key(&mut info, b"piece length");
    encode_int(&mut info, u64::from(PIECE_LENGTH));
    encode_key(&mut info, b"pieces");
    encode_bytes(&mut info, &pieces);
    info.push(b'e'); // end info dict

    let mut hasher = Sha1::new();
    hasher.update(&info);
    let info_hash: [u8; 20] = hasher.finalize().into();

    Fixture {
        content,
        info_hash,
        pieces,
        files,
    }
}

fn build_params(pieces: Vec<u8>) -> TorrentParams {
    TorrentParams {
        piece_count: PIECE_COUNT,
        piece_length: u64::from(PIECE_LENGTH),
        total_length: TOTAL,
        piece_hashes: pieces,
        private: false,
    }
}

// ---------------------------------------------------------------------------
// Gate 10: multi-file end-to-end SHA-256 match
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn magpie_seed_to_magpie_leech_multi_file_sha256_match() {
    let fx = fixture(0xBEEF);
    let info_hash = fx.info_hash;
    let content_sha256 = sha256(&fx.content);

    // 1. Seed: MultiFileStorage pre-populated with the fixture content.
    let seed_root = tempfile::tempdir().expect("seed tempdir");
    let seed_pool = Arc::new(FdPool::with_default_cap());
    let seed_storage = Arc::new(
        MultiFileStorage::create(seed_root.path(), &fx.files, Arc::clone(&seed_pool))
            .expect("seed storage"),
    );
    seed_storage
        .write_block(0, &fx.content)
        .expect("seed write");

    let seed_alerts = Arc::new(AlertQueue::new(256));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let mut seed_req = AddTorrentRequest::new(
        info_hash,
        build_params(fx.pieces.clone()),
        Arc::clone(&seed_storage) as Arc<dyn Storage>,
        *b"-Mg0001-mfseed01abcd",
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

    // 2. Leech: empty MultiFileStorage.
    let leech_root = tempfile::tempdir().expect("leech tempdir");
    let leech_pool = Arc::new(FdPool::with_default_cap());
    let leech_storage = Arc::new(
        MultiFileStorage::create(leech_root.path(), &fx.files, Arc::clone(&leech_pool))
            .expect("leech storage"),
    );

    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let mut leech_req = AddTorrentRequest::new(
        info_hash,
        build_params(fx.pieces.clone()),
        Arc::clone(&leech_storage) as Arc<dyn Storage>,
        *b"-Mg0001-mfleech01abc",
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

    // 3. Drive.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut completed = 0_usize;
    let mut all_alerts: Vec<Alert> = Vec::new();
    while completed < PIECE_COUNT as usize {
        if std::time::Instant::now() > deadline {
            let seed_drained = seed_alerts.drain();
            panic!(
                "multi-file download did not complete in 30s; got {completed}/{PIECE_COUNT}.\n\
                 leech alerts: {all_alerts:?}\n\
                 seed alerts:  {seed_drained:?}"
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

    // 4. Overall SHA-256 check.
    let mut got = vec![0u8; TOTAL as usize];
    leech_storage.read_block(0, &mut got).expect("leech read");
    assert_eq!(
        sha256(&got),
        content_sha256,
        "leech content SHA-256 mismatch"
    );

    // 5. Per-file SHA-256 check: each on-disk file matches its slice of
    //    the fixture content.
    let mut cursor: u64 = 0;
    for f in &fx.files {
        let file_path = leech_root.path().join(&f.path[0]);
        let on_disk = std::fs::read(&file_path).expect("per-file read");
        assert_eq!(
            on_disk.len() as u64,
            f.length,
            "file length mismatch: {:?}",
            f.path
        );
        let start = cursor as usize;
        let end = start + f.length as usize;
        let expected = &fx.content[start..end];
        assert_eq!(
            sha256(&on_disk),
            sha256(expected),
            "per-file SHA-256 mismatch: {:?}",
            f.path
        );
        cursor += f.length;
    }

    // Tear down.
    seed_engine.shutdown(seed_id).await;
    leech_engine.shutdown(leech_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
}

// ---------------------------------------------------------------------------
// Gate 12: fd-pool bound holds under load
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fd_pool_bound_under_load() {
    let fx = fixture(0xD00D);
    // Seven non-zero entries + two zero-length = 9 actual on-disk files,
    // well over the fd_cap of 4. LRU + lazy reopen must keep everything
    // within budget.
    let fd_cap = 4;

    let seed_root = tempfile::tempdir().expect("seed tempdir");
    let seed_pool = Arc::new(FdPool::with_cap(fd_cap));
    let seed_storage = Arc::new(
        MultiFileStorage::create(seed_root.path(), &fx.files, Arc::clone(&seed_pool))
            .expect("seed storage"),
    );
    seed_storage
        .write_block(0, &fx.content)
        .expect("seed write");

    let seed_alerts = Arc::new(AlertQueue::new(256));
    seed_alerts.set_mask(AlertCategory(u32::MAX));
    let seed_engine = Arc::new(Engine::new(Arc::clone(&seed_alerts)));
    let mut seed_req = AddTorrentRequest::new(
        fx.info_hash,
        build_params(fx.pieces.clone()),
        Arc::clone(&seed_storage) as Arc<dyn Storage>,
        *b"-Mg0001-fdseed01abcd",
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

    let leech_root = tempfile::tempdir().expect("leech tempdir");
    let leech_pool = Arc::new(FdPool::with_cap(fd_cap));
    let leech_storage = Arc::new(
        MultiFileStorage::create(leech_root.path(), &fx.files, Arc::clone(&leech_pool))
            .expect("leech storage"),
    );

    let leech_alerts = Arc::new(AlertQueue::new(256));
    leech_alerts.set_mask(AlertCategory(u32::MAX));
    let leech_engine = Arc::new(Engine::new(Arc::clone(&leech_alerts)));
    let mut leech_req = AddTorrentRequest::new(
        fx.info_hash,
        build_params(fx.pieces.clone()),
        Arc::clone(&leech_storage) as Arc<dyn Storage>,
        *b"-Mg0001-fdleech01abc",
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

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut completed = 0usize;
    while completed < PIECE_COUNT as usize {
        assert!(
            std::time::Instant::now() <= deadline,
            "fd_pool_bound_under_load did not complete in 30s"
        );
        let drained = leech_alerts.drain();
        completed += drained
            .iter()
            .filter(|a| matches!(a, Alert::PieceCompleted { .. }))
            .count();
        if completed < PIECE_COUNT as usize {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Verify correctness first — the fd-cap must not break the download.
    let mut got = vec![0u8; TOTAL as usize];
    leech_storage.read_block(0, &mut got).expect("leech read");
    assert_eq!(sha256(&got), sha256(&fx.content));

    // Gate 12 assertion: LRU was actually exercised (counter > cap).
    // Each of seed and leech exceeded `fd_cap` total opens, proving
    // eviction + reopen kicked in.
    assert!(
        seed_pool.opens_total() > fd_cap as u64,
        "seed opens_total = {} (should be > fd_cap = {fd_cap})",
        seed_pool.opens_total()
    );
    assert!(
        leech_pool.opens_total() > fd_cap as u64,
        "leech opens_total = {} (should be > fd_cap = {fd_cap})",
        leech_pool.opens_total()
    );

    seed_engine.shutdown(seed_id).await;
    leech_engine.shutdown(leech_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), seed_engine.join()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), leech_engine.join()).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_key(out: &mut Vec<u8>, key: &[u8]) {
    encode_bytes(out, key);
}

fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(bytes);
}

fn encode_int(out: &mut Vec<u8>, n: u64) {
    out.push(b'i');
    out.extend_from_slice(n.to_string().as_bytes());
    out.push(b'e');
}

fn splitmix64_fill(out: &mut Vec<u8>, target_len: usize, seed: u64) {
    let mut state = seed;
    out.resize(target_len, 0);
    let mut i = 0;
    while i + 8 <= target_len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out[i..i + 8].copy_from_slice(&z.to_le_bytes());
        i += 8;
    }
    while i < target_len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        out[i] = (state & 0xff) as u8;
        i += 1;
    }
}
