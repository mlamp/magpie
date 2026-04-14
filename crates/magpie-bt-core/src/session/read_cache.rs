//! Session-global piece-granular read cache (ADR-0018).
//!
//! Keyed on `(InfoHash, PieceIndex)`; entries hold the full piece as
//! [`Bytes`]. A block served to N peers is `piece.slice(offset..offset+len)`
//! — one disk read, N refcount bumps, zero memcpy.
//!
//! ## Singleflight
//!
//! Concurrent misses for the same piece share a single disk read. The first
//! caller claims the pending slot and issues the read; subsequent callers
//! subscribe to the broadcast and await the result. This prevents the
//! thundering-herd problem where every unchoked peer triggers its own
//! `DiskOp::Read` for a newly-requested piece.
//!
//! ## Store-buffer short-circuit
//!
//! The cache supports a [`ReadCache::insert_verified`] path used by the
//! torrent actor when a piece completes verification and commit. **Promotion
//! ordering** (plan invariant #1): insert into the cache *before* removing
//! the piece from `pending_writes`, never the reverse — otherwise a block
//! request landing between the two operations would re-read from disk a
//! piece we just wrote.
//!
//! ## Bypass
//!
//! [`ReadCache::read_bypass`] is a one-shot path for one-off / non-hot
//! reads that shouldn't pollute the LRU (e.g. resume-time verification).
//! Goes straight to disk without touching the cache at all.
//!
//! ## Eviction
//!
//! Size-based, cheapest-LRU: each entry carries a monotonic access tick.
//! On insert, if over budget, scan the `HashMap` for the minimum tick and
//! evict. `O(N)` per eviction; `N ≤ 256` for a 64 `MiB` cache at 256 `KiB`
//! pieces, so this is cheaper than maintaining a linked list and well-suited
//! to the low-contention read-cache access pattern.

#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::session::disk::{DiskError, DiskOp};

/// `(info_hash, piece_index)` — session-global cache key.
pub type CacheKey = ([u8; 20], u32);

/// Default session-global read-cache budget (ADR-0018).
pub const DEFAULT_READ_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Per-miss singleflight broadcast capacity. Caps the number of concurrent
/// waiters a single pending load can fan out to; beyond this, additional
/// waiters get `lagged` and re-issue (rare; acceptable under extreme
/// fan-out).
const SINGLEFLIGHT_FANOUT: usize = 128;

/// Session-global read cache.
///
/// Clone the [`std::sync::Arc<ReadCache>`] to share across tasks.
pub struct ReadCache {
    inner: Mutex<Inner>,
    cap_bytes: usize,
}

struct Inner {
    entries: HashMap<CacheKey, CacheEntry>,
    total_bytes: usize,
    next_tick: u64,
    pending: HashMap<CacheKey, broadcast::Sender<Result<Bytes, DiskError>>>,
    hits: u64,
    misses: u64,
    singleflight_joins: u64,
    evictions: u64,
    bypass_reads: u64,
}

struct CacheEntry {
    bytes: Bytes,
    last_access: u64,
}

/// Snapshot of cache counters for observability and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Hits served from the cache directly.
    pub hits: u64,
    /// Misses that issued a fresh disk read.
    pub misses: u64,
    /// Misses that joined an in-flight singleflight load (no extra disk I/O).
    pub singleflight_joins: u64,
    /// Entries evicted to make room for new inserts.
    pub evictions: u64,
    /// Reads that bypassed the cache entirely (`read_bypass`).
    pub bypass_reads: u64,
    /// Current number of cached pieces.
    pub entries: usize,
    /// Current total bytes cached.
    pub total_bytes: usize,
}

enum Lookup {
    Hit(Bytes),
    Pending(broadcast::Receiver<Result<Bytes, DiskError>>),
    Miss(broadcast::Sender<Result<Bytes, DiskError>>),
}

impl ReadCache {
    /// Construct a cache with the given byte budget.
    #[must_use]
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                total_bytes: 0,
                next_tick: 0,
                pending: HashMap::new(),
                hits: 0,
                misses: 0,
                singleflight_joins: 0,
                evictions: 0,
                bypass_reads: 0,
            }),
            cap_bytes,
        }
    }

    /// Construct with [`DEFAULT_READ_CACHE_BYTES`].
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_READ_CACHE_BYTES)
    }

    /// Observability snapshot. Cheap; held lock is brief.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned (requires a prior panic inside
    /// a cache critical section — structurally unreachable).
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let inner = self.inner.lock().expect("read cache poisoned");
        CacheStats {
            hits: inner.hits,
            misses: inner.misses,
            singleflight_joins: inner.singleflight_joins,
            evictions: inner.evictions,
            bypass_reads: inner.bypass_reads,
            entries: inner.entries.len(),
            total_bytes: inner.total_bytes,
        }
    }

    /// Fast hit-only probe. Returns `Some` if the piece is cached; `None`
    /// otherwise. Does not trigger a disk read.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    #[must_use]
    pub fn probe(&self, key: CacheKey) -> Option<Bytes> {
        let mut inner = self.inner.lock().expect("read cache poisoned");
        let tick = inner.bump_tick();
        let entry = inner.entries.get_mut(&key)?;
        entry.last_access = tick;
        let bytes = entry.bytes.clone();
        inner.hits += 1;
        Some(bytes)
    }

    /// Get or load a piece. Hits return immediately; misses singleflight
    /// through to a `DiskOp::Read`; concurrent misses for the same key share
    /// the result.
    ///
    /// `piece_offset` + `piece_length` describe the whole piece inside
    /// storage; the cache stores the whole piece, not just a block.
    ///
    /// # Errors
    ///
    /// Propagates [`DiskError`] from the underlying read. Returns
    /// `DiskError::Io` if the disk-op queue is closed (session shutdown).
    pub async fn get_or_load(
        &self,
        key: CacheKey,
        piece_offset: u64,
        piece_length: u32,
        disk_tx: &mpsc::Sender<DiskOp>,
    ) -> Result<Bytes, DiskError> {
        match self.lookup(key) {
            Lookup::Hit(b) => Ok(b),
            Lookup::Pending(mut rx) => rx.recv().await.map_or(Err(DiskError::Io), |r| r),
            Lookup::Miss(tx) => {
                let result = Self::issue_read(disk_tx, key.1, piece_offset, piece_length).await;
                self.finalize_miss(key, &result);
                let _ = tx.send(result.clone());
                result
            }
        }
    }

    /// Read a piece without ever consulting or populating the cache. For
    /// one-off operations (resume verification, integrity re-scan) that
    /// shouldn't evict hot entries.
    ///
    /// # Errors
    ///
    /// Propagates [`DiskError`].
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub async fn read_bypass(
        &self,
        piece: u32,
        piece_offset: u64,
        piece_length: u32,
        disk_tx: &mpsc::Sender<DiskOp>,
    ) -> Result<Bytes, DiskError> {
        self.inner.lock().expect("read cache poisoned").bypass_reads += 1;
        Self::issue_read(disk_tx, piece, piece_offset, piece_length).await
    }

    /// Insert a just-verified piece. Called by the torrent actor when a
    /// `DiskOp::VerifyAndWrite` succeeds, implementing the store-buffer
    /// short-circuit — subsequent peer reads for the same piece hit the
    /// cache instead of re-reading from disk.
    ///
    /// Plan invariant #1: the caller must insert here **before** removing
    /// the piece from any pending-write queue that the `DiskOp::Read` path
    /// checks. Reversing the order creates a momentary invisibility window.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn insert_verified(&self, key: CacheKey, bytes: Bytes) {
        let mut inner = self.inner.lock().expect("read cache poisoned");
        inner.insert_evicting(key, bytes, self.cap_bytes);
    }

    fn lookup(&self, key: CacheKey) -> Lookup {
        let mut inner = self.inner.lock().expect("read cache poisoned");
        let tick = inner.bump_tick();
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.last_access = tick;
            let bytes = entry.bytes.clone();
            inner.hits += 1;
            return Lookup::Hit(bytes);
        }
        if let Some(tx) = inner.pending.get(&key) {
            let rx = tx.subscribe();
            inner.singleflight_joins += 1;
            return Lookup::Pending(rx);
        }
        let (tx, _rx) = broadcast::channel(SINGLEFLIGHT_FANOUT);
        inner.pending.insert(key, tx.clone());
        inner.misses += 1;
        Lookup::Miss(tx)
    }

    fn finalize_miss(&self, key: CacheKey, result: &Result<Bytes, DiskError>) {
        let mut inner = self.inner.lock().expect("read cache poisoned");
        inner.pending.remove(&key);
        if let Ok(bytes) = result {
            inner.insert_evicting(key, bytes.clone(), self.cap_bytes);
        }
    }

    async fn issue_read(
        disk_tx: &mpsc::Sender<DiskOp>,
        piece: u32,
        offset: u64,
        length: u32,
    ) -> Result<Bytes, DiskError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if disk_tx
            .send(DiskOp::Read { piece, offset, length, reply_tx })
            .await
            .is_err()
        {
            return Err(DiskError::Io);
        }
        reply_rx.await.unwrap_or(Err(DiskError::Io))
    }
}

impl Inner {
    const fn bump_tick(&mut self) -> u64 {
        self.next_tick = self.next_tick.wrapping_add(1);
        self.next_tick
    }

    fn insert_evicting(&mut self, key: CacheKey, bytes: Bytes, cap_bytes: usize) {
        // If the entry already exists, replace it without counting as an
        // eviction (store-buffer re-verify path).
        if let Some(old) = self.entries.remove(&key) {
            self.total_bytes -= old.bytes.len();
        }
        let size = bytes.len();
        // Evict until we fit. `>` not `>=` so a piece exactly at the cap
        // still fits (cap is the budget, not a strict less-than).
        while self.total_bytes + size > cap_bytes && !self.entries.is_empty() {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| *k);
            if let Some(vkey) = victim {
                if let Some(v) = self.entries.remove(&vkey) {
                    self.total_bytes -= v.bytes.len();
                    self.evictions += 1;
                }
            } else {
                break;
            }
        }
        // If the piece alone exceeds the cap, drop it (can't evict enough).
        if size > cap_bytes {
            return;
        }
        let tick = self.bump_tick();
        self.total_bytes += size;
        self.entries.insert(key, CacheEntry { bytes, last_access: tick });
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::session::disk::DiskWriter;
    use crate::storage::{MemoryStorage, Storage};
    use std::sync::Arc;

    fn make_storage_with_pattern(total: u64) -> Arc<dyn Storage> {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(total));
        // Write distinct per-byte pattern so each piece has a unique hash.
        let data: Vec<u8> = (0..total).map(|i| (i as u8).wrapping_mul(17)).collect();
        storage.write_block(0, &data).unwrap();
        storage
    }

    #[tokio::test]
    async fn hit_after_load() {
        let storage = make_storage_with_pattern(8192);
        let (writer, disk_tx, _metrics) = DiskWriter::new(storage, 4);
        let _task = tokio::spawn(writer.run());

        let cache = ReadCache::new(1 << 20);
        let key = ([0u8; 20], 0);
        let first = cache.get_or_load(key, 0, 4096, &disk_tx).await.unwrap();
        assert_eq!(first.len(), 4096);
        let s = cache.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.entries, 1);

        let second = cache.get_or_load(key, 0, 4096, &disk_tx).await.unwrap();
        assert_eq!(first, second);
        let s = cache.stats();
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 1);
    }

    #[tokio::test]
    async fn singleflight_coalesces_concurrent_misses() {
        let storage = make_storage_with_pattern(8192);
        let (writer, disk_tx, metrics) = DiskWriter::new(storage, 4);
        let _task = tokio::spawn(writer.run());

        let cache = Arc::new(ReadCache::new(1 << 20));
        let key = ([0u8; 20], 0);
        // Spawn 8 concurrent loads on the same key. Only one disk op should
        // be issued (the rest join via the broadcast).
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&cache);
            let tx = disk_tx.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_load(key, 0, 4096, &tx).await.unwrap()
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        // All 8 returned the same bytes.
        for r in &results[1..] {
            assert_eq!(&results[0], r);
        }
        let s = cache.stats();
        // Exactly one miss; remaining 7 joined singleflight. Racing order
        // may make the "first" arrive before the broadcast fires, so allow
        // `misses` to be 1 or more only if somehow another was issued (which
        // would indicate a bug).
        assert_eq!(s.misses, 1, "singleflight must coalesce concurrent misses");
        assert!(s.singleflight_joins >= 1, "at least one waiter should have joined");
        // DiskMetrics: no verify-and-write happens here, but io_failures
        // must be 0.
        assert_eq!(
            metrics.io_failures.load(std::sync::atomic::Ordering::Relaxed),
            0,
        );
    }

    #[tokio::test]
    async fn bypass_does_not_pollute_cache() {
        let storage = make_storage_with_pattern(8192);
        let (writer, disk_tx, _metrics) = DiskWriter::new(storage, 4);
        let _task = tokio::spawn(writer.run());

        let cache = ReadCache::new(1 << 20);
        let _ = cache.read_bypass(0, 0, 4096, &disk_tx).await.unwrap();
        let s = cache.stats();
        assert_eq!(s.bypass_reads, 1);
        assert_eq!(s.entries, 0, "bypass must not populate cache");
        assert_eq!(s.misses, 0);
    }

    #[tokio::test]
    async fn lru_evicts_cold_entry_when_over_budget() {
        // Cache budget 3 * 1024 bytes; each piece 1024 bytes. Insert 3,
        // access first, insert 4th — second piece (now coldest) evicted.
        let storage = make_storage_with_pattern(8192);
        let (writer, disk_tx, _metrics) = DiskWriter::new(storage, 4);
        let _task = tokio::spawn(writer.run());

        let cache = ReadCache::new(3 * 1024);
        let key = |p: u32| ([0u8; 20], p);
        // Fill.
        cache.get_or_load(key(0), 0, 1024, &disk_tx).await.unwrap();
        cache.get_or_load(key(1), 1024, 1024, &disk_tx).await.unwrap();
        cache.get_or_load(key(2), 2048, 1024, &disk_tx).await.unwrap();
        // Touch piece 0 so piece 1 becomes coldest.
        let _ = cache.probe(key(0)).unwrap();
        // Insert 4th — piece 1 should be evicted.
        cache.get_or_load(key(3), 3072, 1024, &disk_tx).await.unwrap();
        assert!(cache.probe(key(0)).is_some(), "touched piece 0 must survive");
        assert!(cache.probe(key(2)).is_some(), "piece 2 must survive");
        assert!(cache.probe(key(3)).is_some(), "piece 3 must survive");
        assert!(cache.probe(key(1)).is_none(), "piece 1 must be evicted");
        let s = cache.stats();
        assert!(s.evictions >= 1);
    }

    #[tokio::test]
    async fn insert_verified_short_circuits_next_read() {
        // Simulate the store-buffer short-circuit: torrent actor inserts a
        // just-verified piece into the cache; the next `get_or_load` hits
        // without touching disk.
        let storage = make_storage_with_pattern(8192);
        let (writer, disk_tx, _metrics) = DiskWriter::new(storage, 4);
        let _task = tokio::spawn(writer.run());

        let cache = ReadCache::new(1 << 20);
        let key = ([0u8; 20], 7);
        let payload = Bytes::from(vec![0xAAu8; 4096]);
        cache.insert_verified(key, payload.clone());
        // Now a load must hit the cache (no disk read).
        let got = cache.get_or_load(key, 0, 4096, &disk_tx).await.unwrap();
        assert_eq!(got, payload);
        let s = cache.stats();
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 0, "insert_verified must short-circuit");
    }
}
