//! Deterministic torrent + data-file fixture generator for interop
//! scenarios.
//!
//! Builds a synthetic single-file v1 .torrent via the `test-support`
//! generator and writes three artifacts to `--out-dir`:
//!
//! - `fixture.torrent` — bencode metainfo, no announce URL.
//! - `fixture.torrent.with-announce` — copy of above but with
//!   `announce = <ANNOUNCE_URL>` prepended to the outer dict. Points at
//!   the mock tracker so qBittorrent / Transmission can discover magpie.
//! - `fixture.bin` — the content the torrent describes (bit-exact match).
//!
//! Required for the `--features test-support` build flag, which
//! re-exports the semver-exempt synthetic generator.

#![allow(
    clippy::cast_possible_truncation,
    clippy::missing_docs_in_private_items,
    clippy::too_many_lines,
    unreachable_pub
)]

use std::env;
use std::io::Write as _;
use std::path::PathBuf;

use magpie_bt_metainfo::test_support::synthetic_torrent_v1;

fn main() {
    let out_dir: PathBuf = env::var("FIXTURE_OUT_DIR")
        .map(PathBuf::from)
        .expect("FIXTURE_OUT_DIR required");
    let name = env::var("FIXTURE_NAME").unwrap_or_else(|_| "fixture.bin".into());
    let announce_url =
        env::var("FIXTURE_ANNOUNCE").unwrap_or_else(|_| "http://tracker:6969/announce".into());
    let piece_length: u32 = env::var("FIXTURE_PIECE_LENGTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024);
    let piece_count: u32 = env::var("FIXTURE_PIECE_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(320); // ~5 MiB at 16 KiB pieces.
    let seed: u64 = env::var("FIXTURE_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xBEEF);

    std::fs::create_dir_all(&out_dir).expect("mkdir out_dir");
    let synth = synthetic_torrent_v1(&name, piece_length, piece_count, seed);

    // Write raw torrent (no announce).
    let torrent_path = out_dir.join("fixture.torrent");
    std::fs::write(&torrent_path, &synth.torrent).expect("write torrent");

    // Inject announce: the generator produces `d4:infod...ee` — we need
    // to insert `d8:announce<len>:<url>4:infod...ee`. Easiest: re-encode
    // the outer dict with announce prepended.
    let mut with_announce = Vec::with_capacity(synth.torrent.len() + announce_url.len() + 32);
    with_announce.push(b'd');
    with_announce.extend_from_slice(b"8:announce");
    with_announce.extend_from_slice(announce_url.len().to_string().as_bytes());
    with_announce.push(b':');
    with_announce.extend_from_slice(announce_url.as_bytes());
    // Splice the generator's output by stripping its outer `d` and `e`.
    let inner = &synth.torrent[1..synth.torrent.len() - 1];
    with_announce.extend_from_slice(inner);
    with_announce.push(b'e');
    let with_announce_path = out_dir.join("fixture.torrent.with-announce");
    std::fs::write(&with_announce_path, &with_announce).expect("write announce torrent");

    // Write the data file.
    let data_path = out_dir.join("fixture.bin");
    let mut f = std::fs::File::create(&data_path).expect("create data");
    f.write_all(&synth.content).expect("write data");
    f.sync_all().expect("sync data");

    eprintln!(
        "generate_fixture: out_dir={}\n  torrent={}\n  torrent-with-announce={}\n  data={} ({} bytes, {} pieces @ {} B)",
        out_dir.display(),
        torrent_path.display(),
        with_announce_path.display(),
        data_path.display(),
        synth.content.len(),
        piece_count,
        piece_length,
    );
}
