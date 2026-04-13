# 001 — cratetorrent

- **Repo**: https://github.com/mandreyel/cratetorrent
- **Commit**: `34aa13835872a14f00d4a334483afff79181999f`
- **Date**: 2026-04-13

## Crate/module layout

Workspace with two crates: `cratetorrent` (lib) and `cratetorrent-cli` (bin). Library modules: `engine` (orchestrator, mpsc-driven), `torrent` (per-torrent coordinator), `peer` (protocol, one task per connection), `disk` (single async task), `piece_picker` (selection), `download` (in-progress piece tracking), `alert` (event channel), plus `metainfo`, `conf`, `error`. Flow: `engine → torrent → [peer tasks, disk task]`; peers and disk report back via mpsc.

## Piece picker

- Location: `cratetorrent/src/piece_picker.rs:1-190`.
- Struct: `PiecePicker { own_pieces: Bitfield, pieces: Vec<Piece>, missing_count, free_count }`. Each `Piece` has a frequency counter and `is_pending` flag. Pre-allocated.
- **Rarest-first NOT implemented** — current pick is sequential (first-not-owned with freq>0 and not pending). Author note at line 204: "internal data structures will most likely be changed to be more optimal for the rarest-pieces-first algorithm."
- **Endgame**: enters when `free_count == 0` (`all_pieces_picked()`, line 64-65). Flag `in_endgame` flows in `PieceCompletion` to peers (`torrent.rs:748-749`); peers can then pick duplicate blocks via `pick_blocks(..., in_endgame: true)` (`download.rs:78-79`).

## Storage layer

- **No trait abstraction.** Direct file I/O.
- Linux-only: `pwritev`/`preadv` via `nix::sys::uio` in `disk/io/file.rs:1-120` (`TorrentFile::write()`), with a loop for partial writes.
- Writes vectorised through an `IoVecs` helper (`iovecs.rs`) that bounds/splits iovecs to file-slice limits.
- Path: peer → disk command carrying owned `Vec<u8>` → `Disk::write_block()` batches → spawn on blocking pool via `tokio::task::block_in_place()` (`disk/io/torrent.rs:261-264`) → `pwritev` loop.
- Allocations in the write path: `Vec<u8>` per block from peer; `IoVec` slice built per write (`disk/io/torrent.rs:291-294`); BTreeMap insert in `Piece` (`disk/io/piece.rs:45-52`).
- Read cache: LRU (`disk/io/torrent.rs:96`) storing `Vec<CachedBlock>` where each block is `Arc<Vec<u8>>` per piece. No mmap.

## Event/progress model

- **Channels only — no polling.** Unbounded mpsc everywhere.
- `AlertReceiver` (`alert.rs:29`) is the user-facing progress stream.
- Progress: `Alert::TorrentStats { id, stats }` emitted every ~1 s (`torrent.rs:786`); `Alert::TorrentComplete(id)` on finish. Stats include throughput, piece counts, peer info (`torrent/stats.rs`).

## v1 hashing

- **v1 only. No v2/BEP 52.**
- Piece hashes stored as concatenated raw bytes in `Torrent::piece_hashes: Vec<u8>` (`disk/io/torrent.rs:50`). Piece N extracted by slice `[N*20..N*20+20]` and copied into `[u8; 20]` (`get_piece_expected_hash()`, lines 342-344).
- Verification in `Piece::matches_hash()` (`disk/io/piece.rs:67-78`): iterates blocks in BTreeMap order → SHA-1 hasher → compare to `expected_hash: [u8; 20]`. Runs on the blocking pool.

## Hot-path tricks

- `IoVecs` bounding abstraction avoids file-extension overflow during vectored writes.
- `tokio::task::block_in_place()` for hashing + file I/O keeps the reactor free (`disk/io/torrent.rs:255-261`).
- Pre-allocated vectors: `PiecePicker::pieces` via `Vec::resize_with()` (`piece_picker.rs:41`); `PieceDownload` block states via `Vec::resize_with()` (`download.rs:35`).
- BTreeMap for in-progress blocks preserves order for hashing — no sort later.
- Request pipeline uses BDP slow-start (`Q = B * D / 16 KiB`, `DESIGN.md:310-382`) with continuous measurement.
- LRU read cache sized per piece in `Arc<Vec<u8>>` shares buffers across peers without copies.

## Concurrency model

- Three-layer task hierarchy:
  1. **Engine** (`engine.rs:54`): user commands in; spawns disk + torrent tasks.
  2. **Disk** (`disk.rs:30`): single async task, unbounded mpsc, delegates blocking I/O.
  3. **Torrent** (`torrent.rs`, one per torrent): coordinates peers, 1 s tick, tracker.
  4. **Peer** (`peer.rs`, one per TCP connection).
- Sync primitives:
  - `Arc<RwLock<PiecePicker>>` on `TorrentContext` (`torrent.rs:101`).
  - `RwLock<HashMap<PieceIndex, RwLock<PieceDownload>>>` for in-progress downloads (`torrent.rs:113`).
  - Unbounded mpsc between every layer.
  - Sync `Mutex` for read cache (`disk/io/torrent.rs:96`) — never held across await.
- Author TODO (`torrent.rs:111`): "Benchmark whether using the nested locking approach isn't too slow."

## Pain points

- **Metainfo re-encoding for info-hash** (`metainfo.rs:115-117`): serde-bencode round-trip drops unknown fields, so computed info-hash can mismatch original. Author suggests keeping raw bytes (libtorrent "Tide" pattern).
- **Nested `RwLock<HashMap<_, RwLock<_>>>`** flagged for benchmarking (`torrent.rs:111`).
- **Download-pipeline latency hardcoded to 1 s** (`DESIGN.md:336`, issue #44) — should probe dynamically.
- **No upper bound on disk write buffer** (`disk/io/torrent.rs:38-39`, issue #22) — OOM risk under slow disk + fast peers.
- **Slow-start redundancy** noted in `DESIGN.md:384-399` — TCP already solves it.
- **Linux-only** (`README:113`) because `pwritev`/`preadv` via `nix`.

## What magpie should borrow

1. **Task-based architecture with mpsc between layers.** Peer/disk/torrent/engine as separate tasks; no shared mutable god-object. Directly supports our "no `Arc<Mutex<Session>>`" principle.
2. **`IoVecs` bounding helper.** A zero-copy utility for vectored writes that respects file-slice sizes. Small, reusable, worth lifting.
3. **Blocking-pool offload (`spawn_blocking` / `block_in_place`)** for hashing + file I/O. Our DISCIPLINES.md already commits to this; cratetorrent's layout is a clean reference.
4. **Per-piece LRU read cache with `Arc<Vec<u8>>` blocks.** Zero-copy sharing when two peers ask for the same piece.
5. **BDP-driven request pipeline with runtime measurement.** Beats hardcoded queue sizes. Measure, don't guess.

## What magpie should avoid

1. **Nested `RwLock<HashMap<_, RwLock<_>>>`.** The author himself flagged it. Prefer actor-owned state (per-torrent task owns the piece state; peers message it). Aligns with our principles doc.
2. **Unbounded write buffer.** Add a bounded queue + backpressure signal from disk back to peers. Don't accept blocks faster than we can flush.
3. **Re-encoding metainfo to compute info-hash.** Keep the `info` dict raw bytes; hash those. Our metainfo crate must do this from day one.

## ADR seeds

- **ADR 0002 (event bus)**: cratetorrent uses plain mpsc alerts — no broadcast. That works for a single consumer but not our requirement (multiple subscribers via `broadcast`). Note this divergence.
- **Concurrency architecture (new ADR candidate)**: cratetorrent validates the actor + mpsc pattern. Cite here.
