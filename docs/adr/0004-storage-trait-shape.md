# 0004 — Storage trait shape

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: research/SUMMARY.md (anacrolix `PieceHandle` finding), M1 implementation in `crates/magpie-bt-core/src/storage/`

## Context

Originally pencilled in at M0 as "adopt the anacrolix-style hierarchy: `Storage → TorrentStorage → PieceHandle: AsyncReadAt + AsyncWriteAt`." The recommendation came from `docs/research/SUMMARY.md` §2, which was written before any magpie storage code existed — it mapped the anacrolix shape onto an empty codebase.

By M2 kickoff, M1 has shipped a **flat, positional, synchronous** `Storage` trait (`crates/magpie-bt-core/src/storage/traits.rs`):

```rust
pub trait Storage: Send + Sync {
    fn capacity(&self) -> u64;
    fn write_block(&self, offset: u64, buf: &[u8]) -> Result<(), StorageError>;
    fn read_block(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError>;
    fn writev(&self, iov: &[IoVec<'_>]) -> Result<(), StorageError> { /* default */ }
    fn readv(&self, iov: &mut [IoVecMut<'_>]) -> Result<(), StorageError> { /* default */ }
}
```

Two backends: `FileStorage` (file-backed, will gain `pwritev`/`preadv` per ADR-0008) and `InMemoryStorage` (test-only).

The forcing function for M2 is seeding: concurrent read-while-write on the same torrent, a per-peer upload read path, and bounded in-flight backpressure (ADR-0007). A red-team of these needs against the existing flat trait found zero M2 features that require the anacrolix per-piece handle abstraction:

- Concurrent R/W: `&self` on all methods + positional `pwrite`/`pread` → no cursor contention.
- Upload read path: `read_block(offset, buf)` is exactly what the upload pipeline needs; the torrent actor already owns the piece→offset mapping.
- Backpressure: orthogonal to trait shape (it lives in `DiskWriter` + the disk op queue).
- Store-buffer short-circuit (ADR-0018): orthogonal to trait shape.

`PieceHandle` pays off for streaming (M6), mmap/sqlite/S3 backends (M6+), and per-piece metadata hooks — none of which are in M2–M5.

## Decision

Accept the current flat, positional, synchronous `Storage` trait as the magpie storage abstraction through **M5**. Do not migrate to the anacrolix `PieceHandle` hierarchy in M2.

Two invariants are load-bearing and must not regress:

1. **All `Storage` trait methods take `&self`.** Never `&mut self`. This is what lets multiple concurrent readers and writers share a single `Arc<dyn Storage>` without external locking, which in turn is what lets a torrent seed and leech itself simultaneously. It is also what will make the M6 `PieceHandle` migration purely additive — `PieceHandle` can be introduced as a thin view capturing `(Arc<dyn Storage>, piece_offset, piece_length)` that delegates to the existing methods, without reshaping interior signatures. A `&mut self` method introduced between now and M6 turns that migration from additive into invasive.

2. **Vectored I/O is an implementation detail of `FileStorage`, not part of the `Storage` abstraction.** `writev` and `readv` are moved off the trait and onto `FileStorage` as inherent methods. The one existing call-site (a test in `storage/memory.rs`) is migrated to `write_block` or to an inherent method on `InMemoryStorage`. Rationale: a sqlite backend, an S3 backend, or an in-memory backend will never benefit from vectored I/O — `pwritev`/`preadv` is specifically a `FileStorage` optimization via ADR-0008. Leaving it on the trait either forces every future backend to implement scatter/gather or leaves a default that's a noop-equivalent, both of which misrepresent what the abstraction guarantees.

Future backends (mmap, sqlite, S3) implement exactly the same trait: `capacity`, `read_block`, `write_block`. Any backend-specific optimization is an inherent method on the concrete type. Callers that need the optimization hold the concrete type or downcast.

## Consequences

Positive:

- Zero M2 churn in the storage layer. The existing trait, backends, `DiskWriter`, and callers keep working; M2 builds on top.
- The `&self` invariant, stated explicitly, protects concurrent read-while-write (the M2 seeding primitive) without external synchronization.
- The trait surface shrinks: four methods instead of six. Every future backend implements a smaller interface.
- The M6 `PieceHandle` migration is purely additive. `PieceHandle::{read_at, write_at}` becomes sugar over `storage.read_block(piece_offset + offset, buf)`; no existing caller changes.

Negative:

- Callers that wanted vectored I/O through the trait must now hold a concrete `&FileStorage` (or downcast from `Arc<dyn Storage>`). Today there is one such caller (a test). If future hot paths want scatter/gather, they will need specialised code paths in `FileStorage`-aware callers (notably `DiskWriter`). This is a feature, not a defect — vectored I/O only helps `FileStorage`.
- The `Storage` trait stays synchronous. Async translation is deferred to the M6 migration when it buys something real (streaming). Until then, all storage ops run under `tokio::task::spawn_blocking` from the `DiskWriter` — the same pattern ADR-0007 already uses.

Neutral:

- This ADR supersedes the direction recorded in `docs/research/SUMMARY.md` §2. The summary is a snapshot of pre-code research; updating it would obscure that history. The research remains useful as reference; where it conflicts with shipped code, shipped code wins.

## Alternatives considered

- **Migrate to anacrolix `Storage → TorrentStorage → PieceHandle` in M2** (the originally pencilled direction). Rejected: the migration cost is non-trivial (async plumbing, per-torrent handle lifecycle, rewriting every `DiskOp` call-site) and buys nothing M2 needs. The premature abstraction rule applies — the hierarchy pays off at M6, design it when we build M6.
- **Keep `writev`/`readv` on the trait** with default implementations. Rejected: signals that vectored I/O is first-class across all backends when it is only ever a `FileStorage` optimization, and adds surface area every future backend must reason about (even if only to ignore).
- **Switch the trait to async now** (`async fn read_block`). Rejected for M2: `spawn_blocking` from `DiskWriter` already moves I/O off the runtime (ADR-0007). Making the trait async forces every impl to deal with Send futures and offers no M2 performance win. Revisit at M6 alongside the `PieceHandle` introduction.
