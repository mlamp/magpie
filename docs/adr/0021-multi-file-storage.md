# 0021 — Multi-file storage layout

- **Status**: proposed
- **Date**: 2026-04-18
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/` (rtorrent's library), ADR-0004 (storage trait shape)

## Context

BEP 3 info dicts are either single-file (`length`) or multi-file (`files[].{path, length}`). Magpie's metainfo parser already handles both (`FileListV1::Multi` at `crates/magpie-bt-metainfo/src/types.rs:94`), but the only shipped `Storage` impl — `FileStorage` (`crates/magpie-bt-core/src/storage/file.rs:27`) — is single-file-backed. A consumer wanting to download any real torrent (movies, software releases — essentially every non-ISO torrent) must hand-roll their own `Storage` impl to map logical torrent offsets onto a directory of files.

Research evidence (`rakshasa/libtorrent`, the library rtorrent uses):

- `FileList::create_chunk(offset, length)` (`src/data/file_list.cc:600`) binary-searches for the starting file, iterates files covering the range, emits a composite `Chunk { Vec<ChunkPart> }`. Each `ChunkPart` targets one `(fd, file_offset, len)`.
- Per-file metadata (`File`: `src/data/file.h:9`) is separate from the fd (`FileManager`: `src/data/file_manager.h:11`) — a bounded LRU pool with lazy re-open via `File::prepare()`. rtorrent never opens fds itself; it passes paths and lengths to the library and lets the library own the lifecycle.
- Piece-crosses-file-boundary writes are atomic at the *verified-piece* granularity: both file regions are written before hash verification runs (`HashChunk::perform()`: `src/data/hash_chunk.cc:23`), then `FileList::mark_completed()` flips a single bit.

The same seam applies to magpie. Our `Storage` trait (ADR-0004) is already `&self` + positional: `write_block(offset, buf)` hides whether the backend is one file, many files, or a blob store. Multi-file is a new `Storage` impl, not a new abstraction.

## Decision

Add a new `MultiFileStorage` impl of the existing `Storage` trait. It owns a sorted list of per-file entries and a bounded fd pool. Single-file `FileStorage` is unchanged. `Storage` trait is unchanged.

### Shape

```rust
pub struct MultiFileStorage {
    entries: Vec<FileEntry>,        // sorted by torrent_offset, ascending
    root: PathBuf,                  // canonical, pre-existing directory
    capacity: u64,                  // sum of entries[].length
    fd_pool: Mutex<FdPool>,         // bounded LRU
}

pub struct FileEntry {
    path: PathBuf,                  // relative to root, validated
    torrent_offset: u64,            // [torrent_offset, torrent_offset + length)
    length: u64,
}

pub struct FileSpec {               // caller-supplied (from metainfo)
    pub path: Vec<String>,          // BEP 3 info.files[*].path
    pub length: u64,
}

impl MultiFileStorage {
    pub fn create(root: impl AsRef<Path>, files: &[FileSpec], fd_cap: usize) -> Result<Self, StorageError>;
    pub fn open(root: impl AsRef<Path>, files: &[FileSpec], fd_cap: usize) -> Result<Self, StorageError>;
    pub fn root(&self) -> &Path;
}
```

### Load-bearing invariants

1. **Entries are sorted by `torrent_offset` ascending, with no gaps or overlaps.** `entries[i].torrent_offset + entries[i].length == entries[i+1].torrent_offset`. Constructor validates this. Zero-length entries are allowed (they occupy one position with `length = 0` and contribute nothing to reads/writes but still get a file on disk).
2. **Path validation is fail-closed at construction.** Each `FileSpec.path` component must be `Component::Normal` after `Path::components()` — reject `..`, `/`, `.`, empty, and on Windows `Prefix`. Each final joined path must resolve to within `root` without symlink-following. A duplicate path across entries is rejected (two files mapping to the same on-disk path is nonsense).
3. **All `Storage` trait methods remain `&self`** (ADR-0004 invariant #1). `fd_pool` uses `Mutex<FdPool>` for interior mutability. The `Storage` trait methods themselves never take `&mut self`.
4. **Vectored I/O is not on the trait** (ADR-0004 invariant #2). If per-file scatter/gather becomes a hot path, it lives on `MultiFileStorage` as an inherent method.

### Offset mapping

`write_block(offset, buf)` and `read_block(offset, buf)` both use the same routine:

1. Binary-search `entries` for the starting file (`entries.partition_point(|e| e.torrent_offset + e.length <= offset)`).
2. Walk forward, for each covered entry compute the `(file_offset, slice_len)` contribution and issue one `pread`/`pwrite` via the fd pool.
3. Stop when `buf` is exhausted.

Zero-length entries are skipped transparently by the walk.

### Fd pool

- Default cap: **256** (covers the typical torrent; the 99th-percentile torrent has <100 files).
- Eviction: LRU on open. When the pool is full and a new file must be opened, the least-recently-used entry is `close()`d (`drop(File)`).
- Lazy open: first read/write to a file opens it via `OpenOptions::new().read(true).write(true).open(path)`. Subsequent ops hit the cache.
- Thread-safety: `Mutex<FdPool>` where `FdPool` holds `HashMap<FileIdx, Arc<File>>` + LRU order. Critical section: lookup / insert / evict. `Arc<File>` is returned to the caller so the I/O itself runs outside the mutex. `File::read_at` / `write_at` are reentrant on a shared fd under `&File` (the stdlib wrappers for `pread`/`pwrite` take `&self`).

### Pre-allocation

`create()` calls `File::set_len(length)` per entry. On modern filesystems (APFS, ext4 default, NTFS) this is sparse — O(1), no block allocation until a write. Callers that want eager allocation can issue a zero-write post-construction; not M2 scope.

### Delete

`Storage::delete` unlinks every `entries[].path` and prunes any directory left empty below `root`. `root` itself is never removed.

### Piece-crosses-file-boundary atomicity

No new work. `DiskOp::VerifyAndWrite` (session/disk.rs) already carries the whole piece buffer; the existing write-then-verify sequence covers multi-file naturally. If the process crashes mid-piece, resume-from-disk-scan (M1 machinery) finds the piece unverified and re-downloads it. This is the same contract `FileStorage` already provides.

## Consequences

Positive:

- Consumers download any real-world torrent with no custom `Storage` impl — the primary gap keeping magpie from "works for anything but a single-file ISO" disappears.
- The `Storage` trait remains unchanged: no ripple through `DiskWriter`, `TorrentActor`, or the read cache. ADR-0004's "additive to M7's `PieceHandle`" guarantee holds.
- Matches rakshasa's proven API shape (consumer passes paths, library owns fds) — rtorrent has shipped this model for ~20 years.
- Path safety enforced at construction: no first-write footgun. A malformed torrent fails before any bytes hit disk.

Negative:

- New failure mode: fd pool exhaustion under pathological torrents (10,000+ files + ≥16 KiB piece size + heavy concurrent reads). Mitigated by LRU + lazy reopen. Worst case: extra `open()` syscalls on pool thrashing; no correctness cost.
- `Mutex<FdPool>` is a contention point on the hot path when the pool size is near-saturated. Acceptable for M2 (measure first). If flamegraphs show it, the `Mutex` can become a sharded lock or per-entry `Arc<RwLock<Option<File>>>` without changing the public API.
- Pre-allocation relies on sparse-file support. On filesystems without it (very rare: FAT32), `set_len` on a large file materializes zero blocks upfront. Acceptable — same behavior as `FileStorage::create` today.

Neutral:

- Single-file torrents continue to use `FileStorage`. Consumers choose at `AddTorrentRequest::storage` construction time based on parsed metainfo. No runtime mode-switch.
- A convenience bridge (`MultiFileStorage::from_info_v1(root, info)`) can live in `magpie-bt-core` to keep `magpie-bt-metainfo` dependency-free. Decided at implementation time.

## Alternatives considered

- **Expose primitives (open + offset math) and let the consumer assemble multi-file storage.** Rejected: every consumer of a general-purpose BT library wants this, and rakshasa's code is clear evidence that the assembly is error-prone (boundary splitting, fd pooling, path safety). Making each consumer re-invent it is hostile.
- **Slot multi-file into M7's "pluggable storage: mmap, sqlite, S3" bucket.** Rejected: M7 is 4+ milestones away. Between now and then magpie is unusable for any real-world torrent that isn't a single-file ISO — that blocks the M2 "consumer-integration ready" bar in practice, even if not in letter.
- **Per-`FileEntry` `Arc<RwLock<Option<File>>>` with no central pool.** Rejected for M2: unbounded fd usage. A 10,000-file torrent at steady-state would hold 10,000 open fds. The LRU cap forces a bounded working set at the cost of occasional reopens. Can be revisited if profiling shows the mutex as a bottleneck before the fd count becomes one.
- **Extend the `Storage` trait with a `create_chunk` method returning a scatter/gather handle (anacrolix-style).** Rejected: violates ADR-0004 §Invariant 2 (scatter/gather is a backend detail, not an abstraction). Multi-file offset math can live entirely inside `MultiFileStorage::write_block` without leaking to the trait.
