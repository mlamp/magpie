# 0018 — Read cache

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: rasterbar v1.x piece-aligned disk cache (research/003 §3), cratetorrent piece LRU (research/001), Arvid's v2 rip-out rationale (research/003 §3 "mmap + kernel page cache")

## Context

On M2's upload path, a popular piece can be requested by 20+ leechers in quick succession. Without caching above `FileStorage`, each block-grained wire `Request` becomes one `pread(16 KiB)` syscall — 16× per piece at the minimum and up to `20 × blocks_per_piece` at peak fan-out. That throws away two obvious wins:

1. **Piece locality.** A peer requesting block `[0..16)` of piece 42 will, with overwhelming probability, request `[16..32)` next. A peer fetching adjacent blocks from the same piece pays one `pread` per block instead of one `pread` per piece.
2. **Cross-peer fan-out.** N peers requesting the same block on the same piece should pay one `pread`, N refcount bumps, and N wire writes — not N `pread`s.

Rasterbar v2 deleted its user-space disk cache and relies on the kernel page cache (file-backed mmap). Magpie is on `FileStorage` + `pread` through M5 (ADR-0004: mmap is M6), so we don't yet have that fallback. ADR-0017 assumes a read cache exists both for fan-out and as the short-circuit target when a block is still sitting in the disk-write pending set (ADR-0007's [enqueued, flushed] window).

Three design questions:

1. **Piece-granular or block-granular?** A piece is 256 KiB–4 MiB; a block is 16 KiB (v2 invariant, enforced in v1 per `PROJECT.md`).
2. **Per-torrent or session-global budget?** The M2 plan committed to session-global, mirroring the session-wide disk-write budget (ADR-0007).
3. **How does the store-buffer short-circuit interact?** A `DiskOp::Read` for a piece whose `VerifyAndWrite` is still queued in `DiskWriter` must be served without touching disk.

## Decision

Magpie ships a session-global piece-granular LRU at `magpie-bt-core/src/session/read_cache.rs`.

### Cache shape

```rust
pub struct ReadCache {
    entries: Mutex<LruCache<(InfoHash, PieceIndex), Bytes>>,
    inflight: Mutex<HashMap<(InfoHash, PieceIndex), Shared<oneshot::Receiver<Bytes>>>>,
    capacity_bytes: u64,   // default 64 MiB (see §Budget sizing)
    current_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    short_circuits: AtomicU64,  // hits via the DiskWriter pending set
    bypasses: AtomicU64,        // single-block misses served without populating
}
```

- **Key**: `(InfoHash, PieceIndex)`. Piece-granular — one entry per piece, not per block.
- **Value**: `bytes::Bytes` holding the verified piece payload. Slicing a block out is `piece.slice(offset..offset + len)`, which is ref-counted and zero-copy. A wire `ServeBlock` holds a `Bytes` slice into the cached piece; dropping it decrements a refcount.
- **Singleflight**: the `inflight` map deduplicates concurrent misses for the same piece. The first miss registers a `oneshot::Sender` and begins the pread; subsequent misses for the same key await the `Shared` receiver. Prevents N peers on cold-start from each issuing their own pread for the same piece.
- **Budget sizing**: 64 MiB session-global default, matching the ADR-0007 disk-write budget for symmetry. At typical piece sizes the cache holds:
  - 256 KiB pieces → 256 entries
  - 1 MiB pieces → 64 entries
  - 4 MiB pieces → 16 entries

  Operators should raise this for torrents with large pieces and wide leecher fan-out. `ReadCacheConfig::capacity_bytes` is the knob. Eviction is strict LRU when `current_bytes + new_piece_len > capacity_bytes`.

Piece-granular over block-granular for two reasons:

1. One `pread(piece_len)` to populate the cache is strictly cheaper than `blocks_per_piece` `pread(16 KiB)` calls on the same file.
2. A block-granular cache at 64 MiB = 4096 entries has 16–256× the LRU bookkeeping overhead of a piece-granular cache at the same byte budget (16× at 256 KiB pieces, 256× at 4 MiB pieces).

The memory accounting stays in bytes (not entries) so piece size heterogeneity doesn't surprise the budget.

### Read path

`DiskOp::Read { info_hash, piece, offset, length, completion_tx }` in `DiskWriter::run` resolves as:

1. **Store-buffer short-circuit.** If the piece is in `DiskWriter::pending_writes: HashMap<(InfoHash, PieceIndex), Bytes>` — i.e. `VerifyAndWrite` enqueued but not yet flushed — slice out the block and return. No cache touch, no disk touch. Bumps `short_circuits`.
2. **Read cache hit.** `ReadCache::get((info_hash, piece))` → slice and return. Bumps `hits`.
3. **Singleflight coalesce.** Take the `inflight` lock: if another read for the same key is in progress, register a waiter on its `Shared<oneshot>` and release the lock. When the future resolves, slice from the returned `Bytes`.
4. **Miss decision — cache or bypass.** Still holding the `inflight` lock briefly to decide:
   - If there is already another `DiskOp::Read` pending for the same piece in the DiskWriter's work queue (observable because the singleflight slot is about to be claimed by multiple waiters), read the whole piece, insert into `ReadCache`, resolve all waiters. Fan-out will pay for itself. Bumps `misses`.
   - If this is the only outstanding request for the piece, **bypass the cache**: read only the requested block (16 KiB) via `storage.read_block(piece_offset + offset, &mut buf[..length])`. Do not insert into the cache. Bumps `bypasses`. Avoids 4 MiB-per-request cache pollution for one-off reads from peers about to disconnect.
5. **Cache populate path (taken when fan-out or sweep is likely).** Release the `inflight` lock before calling `storage.read_block`. After the pread completes: re-acquire the cache lock, insert, remove the singleflight slot, resolve the `oneshot::Sender` (broadcasts to waiters), slice, and return.

The `Mutex<LruCache>` is **never held across the pread**. Lock is taken briefly for get / put / eviction bookkeeping only. Singleflight coordination uses a separate `inflight` map so cache-hit reads and miss-resolution paths don't contend on the same lock.

The returned value in all paths is a `Bytes` slice (refcounted) sent back via `completion_tx`.

### Write path hook

To make the store-buffer short-circuit work without a per-piece memcpy, **`DiskOp::VerifyAndWrite` changes its buffer type from `Vec<u8>` to `Bytes`** (ADR-0007 amendment). The producer (torrent actor) assembles blocks into a `BytesMut` and freezes it into `Bytes` before enqueueing. The `DiskWriter` then shares the same `Bytes` handle between `pending_writes` and the verify+write worker — `clone()` on `Bytes` is a refcount bump, not a piece-sized memcpy. At a sustained 100 pieces/s download rate on 4 MiB pieces, this is the difference between 400 MiB/s of memcpy overhead and zero.

`DiskWriter` maintains `pending_writes: HashMap<(InfoHash, PieceIndex), Bytes>`:

- **On `DiskOp::VerifyAndWrite` enqueue**: `pending_writes.insert((info_hash, piece), buffer.clone())` — refcount bump, not memcpy.
- **After SHA-1 verify + storage write succeeds** (ordering is load-bearing):
  1. **Insert into `ReadCache` first.**
  2. **Then** remove from `pending_writes`.

  Reversed ordering leaves a brief window where the piece is in neither map and a concurrent `DiskOp::Read` would fall through to a spurious disk pread. The chosen ordering permits a brief double-presence, which is harmless: both paths return the same `Bytes`, same refcount, same data.
- **On verify failure**: remove from `pending_writes`, do not insert into `ReadCache`.

This gives us one consistent invariant: **a verified piece is always readable without touching disk** for the cache's eviction lifetime.

## Consequences

Positive:

- Fan-out: N peers requesting the same block → one disk read, N refcount bumps. Directly addresses the hot-torrent seeding case.
- Piece locality: a peer sweeping adjacent blocks pays one `pread` per piece, not per block.
- Store-buffer short-circuit: downloading-and-seeding the same torrent (common in `*arr` workflows) serves upload reads from the write buffer with zero disk I/O during the [enqueued, flushed] window.
- `Bytes`-based zero-copy slicing: a block served to three peers is still one heap allocation total. No memcpy per `ServeBlock`.
- Budget is bounded and session-wide, matching ADR-0007's shape. Per-torrent fairness is provided by the bandwidth shaper (ADR-0013), not a second-level cache budget.
- Telemetry fields (`hits`, `misses`, `short_circuits`) feed the Prometheus exporter for free.

Negative:

- Defeated by cold torrents with random access patterns. The `bypass` path protects us from reading 4 MiB per one-off 16 KiB request — so a scan-and-disconnect peer just costs one 16 KiB pread. If the cache is useful (fan-out / piece-sweep), singleflight ensures one piece-sized pread regardless of waiter count. The kernel page cache catches any residual repeated-block reads at the OS layer.
- Holds `Bytes` references past a piece's natural lifetime: if a `ServeBlock`'s refcount outlives the cache eviction (slow peer on a fast-churning cache), the underlying buffer stays alive until the last wire sink flushes. Memory pressure is bounded by the ready-queue watermark sum across peers (ADR-0017), which lives in the same byte budget space — but note this is *additional* to the 64 MiB cache (not carved out of it).
- One session-wide `Mutex`. Piece fetches are not on the hot path (blocks are sliced cheaply and `DiskWriter` already serialises disk work), but if contention shows up in flamegraphs, switch to `DashMap` or sharded-LRU. Not a M2 concern.

Neutral:

- `pending_writes` shares `Bytes` handles with the verify+write worker — no duplication. The only cost is the `HashMap` entry itself. Total session-wide hot set: 64 MiB disk-write budget (ADR-0007) + 64 MiB read cache = 128 MiB, comfortably within the anacrolix-lineage memory envelope (and still an order of magnitude below a typical *arr workflow's page-cache footprint).

## Alternatives considered

- **Block-granular cache.** Rejected: 8–256× the LRU overhead for the same byte budget, and breaks the one-pread-per-piece win that's the whole point.
- **No read cache; rely on kernel page cache.** Rasterbar v2's choice, via `mmap`. Rejected for M2 because magpie uses `pread` through M5; the kernel cache is still there below us but doesn't give us the zero-copy `Bytes` fan-out we want for cross-peer sharing. Revisit at M6 with mmap.
- **Per-torrent cache budget.** Rejected: same reasoning as ADR-0007's session-wide shape — the bandwidth shaper (ADR-0013) already rate-limits per-torrent work upstream, so a second cap on the cache layer re-prices the same constraint and introduces an unresolved "what's each torrent's fair share?" policy question.
- **Integrate the cache into `FileStorage` instead of `DiskWriter`.** Rejected: `FileStorage` is sync and backend-agnostic (ADR-0004); a cache bolted onto it would have to be the same shape for every future backend, and couldn't see the `DiskWriter`'s pending set for the store-buffer short-circuit. The cache lives at the session layer, one step above storage, where it can see both in-flight writes and disk reads.
- **`Arc<[u8]>` instead of `Bytes`.** Equivalent for the fan-out case but `Bytes::slice` gives zero-cost subranges; with `Arc<[u8]>` the ServeBlock must carry `(Arc<[u8]>, offset, len)` explicitly. The `bytes` crate is already a transitive dep via tokio; no new cost.
- **Populate on every miss (no bypass).** Rejected: a sweeping peer requesting one block from each of 100 different pieces would read 100 × piece_size from disk (up to 400 MiB on 4 MiB pieces) and immediately evict them via LRU churn. The bypass path at step 4 keeps cache admission decisions correlated with fan-out likelihood.
- **`buffer.clone()` on `Vec<u8>` before `Bytes::from`.** The first draft assumed this was free; it is not — `Vec::clone` is a full piece-sized memcpy (256 KiB–4 MiB). At 100 pieces/s that is 25–400 MiB/s of pointless copy. Fix: make `DiskOp::VerifyAndWrite::buffer` a `Bytes` at the producer, share refcounted between `pending_writes` and the verify+write path. Recorded as an ADR-0007 amendment in the write-path section above.
- **Default budget 32 MiB.** First-draft value; raised to 64 MiB to match the write budget after noticing the "8 pieces at 4 MiB" math undersized the cache for wide-fan-out seeding.
