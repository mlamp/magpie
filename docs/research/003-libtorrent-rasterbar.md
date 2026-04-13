# Research: libtorrent-rasterbar (Arvid Norberg)

**Date**: 2026-04-13  
**Repository**: https://github.com/arvidn/libtorrent  
**Commit SHA**: 0837365feb52993d47e4e26c90433ad469b43f54

---

## 1. Alert System (Non-Blocking Event Ring Buffer)

### Type Hierarchy & Subscription Model

**Alert hierarchy** is dispatch-by-enum rather than polymorphic inheritance abuse. Located in:
- `/include/libtorrent/alert.hpp` (lines 189–330): Abstract `alert` base class with 25+ concrete subtypes defined in `alert_types.hpp`
- Each alert type has a unique `static constexpr alert_type` integer and belongs to one or more categories (error, peer, tracker, storage, dht, stats, etc.) defined as bitfield flags
- Consumers call `session::pop_alerts()` which drains the alert buffer and returns a `std::vector<alert*>`

**Alert generation**: Producers call `alert_manager::emplace_alert<AlertType>(args...)` which emplaces directly into the queue. No allocation overhead beyond the heterogeneous queue.

**Subscription model**: **Poll-based**, not push. The consumer must call `pop_alerts()` periodically. However, a callback hook exists via `set_notify_function()` (`alert_manager.hpp:124`) to wake the application's event loop when the queue transitions from empty → non-empty.

**Ring buffer structure** (`include/libtorrent/aux_/alert_manager.hpp`, lines 156–171):
```cpp
// Double-buffered heterogeneous queue, toggled on poll
aux::array<heterogeneous_queue<alert>, 2> m_alerts;  // one for producer, one for consumer
std::uint32_t m_generation = 0;  // generation counter, toggled by get_all()

// Stack allocator for variable-length alert payloads (strings, etc.)
aux::array<stack_allocator, 2> m_allocations;
```

When `get_all()` is called (`src/alert_manager.cpp:110–132`):
1. Scan active queue
2. If any alerts were dropped due to queue size limit, emit `alerts_dropped_alert` with a bitset of dropped types
3. Return pointers to all alerts
4. **Swap buffers** (`m_generation += 1`)
5. Clear the new producer queue

### Backpressure

When the queue reaches `m_queue_size_limit` (default ~100 alerts), new alerts are dropped and a bit is set in `m_dropped` bitset. The next `get_all()` call emits `alerts_dropped_alert` so the consumer knows it missed data. **No unbounded queueing.**

High-priority alerts (e.g., `save_resume_data` responses) get double the limit before being dropped (`emplace_alert` line 81: `queue.size() / (1 + static_cast<int>(T::priority))`).

---

## 2. Piece Picker (Rarest-First + Speed-Class Affinity)

### Data Structure

Located: `/src/piece_picker.cpp` (2600+ lines), `/include/libtorrent/piece_picker.hpp`

**Core structures** (`piece_picker.hpp:113–233`):
- `m_piece_map[piece_index]`: per-piece metadata (peer_count, priority, download_queue index, state)
- `m_downloads[download_queue]`: 8 separate queues for pieces in states: {open, downloading, full, finished, zero_prio, …}
- `m_pieces[index]`: sorted list of piece indices, reordered by priority
- `m_priority_boundaries[j]`: indices into `m_pieces` marking where each priority level begins (enables O(1) bucket selection)
- `m_block_info[piece_idx * blocks_per_piece]`: per-block state (peer, request count, finished status) — reused indices from a free list

**Rarest-first ranking** (`piece_picker.cpp:2244–2258`): 
```cpp
// iterates m_pieces in order (which is sorted by peer count / rarity)
for (piece_index_t i : m_pieces) {
    if (!is_piece_free(i, pieces)) continue;
    ret |= picker_log_alert::rarest_first;
    num_blocks = add_blocks(i, pieces, interesting_blocks, …);
}
```

Rarity is tracked as `piece_pos::peer_count`, incremented on HAVE/BITFIELD and used to reorder `m_pieces` list. Pieces with fewer peers appear first.

### Speed-Class Affinity (Piece Extent Affinity)

The picker tracks **4 MiB extents** of contiguous pieces (`piece_picker.cpp:145–146`):
```cpp
constexpr int max_piece_affinity_extent = 4 * 1024 * 1024 / default_block_size;
```

**When enabled** (flag `piece_extent_affinity`, line 169), the picker maintains `m_recent_extents` — a list of recently-started piece extents. When picking for a peer:

1. **Check recent extents first** (`piece_picker.cpp:2211–2241`): If a peer started downloading from an extent, prefer to keep adding blocks from that same extent
2. **Logic**: Iterate `m_recent_extents`, find pieces in each extent that are free, add blocks from them until the desired count is reached
3. **Fallback**: If recent extents are exhausted or fully downloaded, fall back to rarest-first across all pieces
4. **Eviction**: Once all pieces in an extent are complete, remove it from `m_recent_extents`

This avoids the "rarity roulette" where a peer bounces between many partial pieces, reducing disk seek time and improving cache locality.

### Endgame Mode

Not in the piece picker itself. Instead, endgame is a **peer connection state** (`peer_connection.hpp:254`):
```cpp
bool m_endgame_mode:1;
```

When triggered (by torrent, not picker):
- Peer enters endgame if we've requested nearly all blocks and download speed has slowed
- In endgame, desired queue size is clamped to 1 instead of the normal multi-block deep queue
- Multiple peers can request the same block (redundantly) to speed up final blocks

---

## 3. Disk I/O Pool (Off Net Loop)

Located: `/src/mmap_disk_io.cpp` (1500+ lines), `/include/libtorrent/aux_/disk_io_thread_pool.hpp`

### Thread Pool Architecture

**Two-tier design** (`mmap_disk_io.cpp:371–374`):
```cpp
job_queue m_generic_io_jobs;
aux::disk_io_thread_pool m_generic_threads;
job_queue m_hash_io_jobs;
aux::disk_io_thread_pool m_hash_threads;
```

Generic jobs (read, write, move, delete) and hash jobs are pooled separately. Max threads configurable per pool.

**Job queue** (lines 241–270): Per-pool `job_queue` struct with:
- `jobqueue_t m_queued_jobs`: Intrusive tail queue of `mmap_disk_job` structs
- `std::condition_variable m_job_cond`: Woken when jobs arrive
- Thread pulls job, executes, submits completion callback back to network loop's `io_context`

### Work Submission & Completion

Producers call `async_read()`, `async_write()`, etc. (lines 715, 838):
```cpp
mmap_disk_io::async_read(storage_index_t storage, peer_request const& r,
    std::function<void(disk_buffer_holder, storage_error const&)> handler, ...)
```

Internally:
1. Allocate `mmap_disk_job` from `m_job_pool` (pre-allocated)
2. Enqueue to appropriate job queue
3. Notify `m_generic_threads.job_queued(queue_size)`

**Completion callback** (`mmap_disk_io.cpp:add_completed_jobs`):
- Disk thread finishes work, stores result in job struct
- Appends job to `m_completed_jobs` queue (protected by mutex)
- If queue was empty, posts a message to network loop's `io_context` to drain and execute callbacks

This prevents any disk stall from blocking the network reactor.

### Vectorized I/O

**No explicit `pwritev`/`preadv` found in the current codebase**. The mmap implementation instead:
- Memory-maps file storage (`mmap_storage.hpp`)
- Reads/writes directly to mapped memory
- Relies on kernel page cache for coalescing (less efficient than scattered I/O for many small blocks, but simpler)

For posix disk I/O backend (`src/posix_disk_io.cpp`), individual `pwrite`/`pread` calls are made per block. Vectorization was removed in favor of memory mapping's simplicity.

### Queue Bounds & Backpressure

Job queue is **unbounded**. However:
- Thread pool scales up to `max_threads` (default ~4) in response to queue depth
- Disk buffer pool has a fixed size; when full, write jobs block the producer thread
- Hash jobs can be rate-limited if `m_hash_threads` is set to 0 (runs hash on-demand on requesting thread)

---

## 4. BEP 52 (v1/v2 Hash Abstraction)

### Merkle Tree Storage & Verification

Located: `/include/libtorrent/aux_/merkle_tree.hpp` (lines 83–150), `/src/merkle_tree.cpp`

**Core structure** (`merkle_tree.hpp:83–150`):
```cpp
struct merkle_tree {
    // Sparse representation: only interior nodes + verified leaf hashes
    std::map<int, sha256_hash> m_tree;  // node_idx → hash
    std::vector<bool> m_verified;       // leaf node verification status
    
    // Load tree, including sparse partial layers
    void load_sparse_tree(span<sha256_hash const> t, std::vector<bool> const& mask, 
                         std::vector<bool> const& verified);
    
    // Add hashes from peers with proof path
    boost::optional<add_hashes_result_t> add_hashes(
        int dest_start_idx, piece_index_t::diff_type file_piece_offset,
        span<sha256_hash const> hashes, span<sha256_hash const> uncle_hashes);
    
    // Verify block hash and validate up the tree
    std::tuple<set_block_result, int, int> set_block(int block_index, 
                                                     const sha256_hash& h);
};
```

**Hash picker** (`include/libtorrent/hash_picker.hpp:35–100`): Manages which block hashes to request from peers. Maintains a bitfield of verified vs. unverified blocks per file. On peer responses, validates block hashes against merkle tree; if valid, marks the block (and validates all ancestors).

**v1 vs. v2 abstraction**: Both hash types (SHA1 for v1, SHA256 for v2 leaf/interior) are handled via template instantiation. The torrent_info layer exposes `file_storage::root(file_idx)` for v2 file roots and `add_torrent_params::merkle_trees` for partial layer supply.

---

## 5. Pain Points & Design Regrets

### Long-Standing Issues (Grepped from code)

1. **Affinity precedence** (`piece_picker.cpp:2209`):
   ```cpp
   // TODO: Is it a good idea that this affinity takes precedence over piece priority?
   ```
   Piece extent affinity can suppress high-priority pieces if a recent extent is active.

2. **Configurable limits** (`piece_picker.cpp:1994`):
   ```cpp
   // TODO: make the 2048 limit configurable
   ```
   Hard-coded 2048-block request queue cap; should be tunable.

3. **Ring buffer generation tracking** (`src/alert_manager.cpp:80`):
   ```cpp
   // TODO: keep a count of the number of threads waiting. Only if it's > 0 notify them
   ```
   Notification strategy is simplistic; notifies all even if no threads are waiting.

4. **Endgame undefined**: No formal definition or config for when endgame activates. Left to each torrent to decide based on heuristics (e.g., >95% complete + low speed).

### C++-Specific Overengineering

- **Heterogeneous queue**: Uses type erasure and placement-new to store polymorphic alerts. Rust could use a simpler `enum` with discriminant.
- **Stack allocator**: Inline string storage to reduce allocations. In Rust, `String` or `Arc<str>` suffices; the allocation overhead is negligible.
- **Intrusive job queue**: Uses pointer intrusion (`tailqueue<job>`) for zero extra allocation. Rust's `VecDeque` or `crossbeam::queue::ArrayQueue` is cleaner.
- **Double-buffering for alerts**: Swap on poll works but requires generation counter + bitfield. A simple bounded MPMC queue (e.g., `crossbeam-channel`) is simpler.

---

## 6. What Magpie Should Borrow

1. **Typed alert enum with categories**  
   - Define `TorrentEvent` as a flat enum (not trait objects)
   - Bitmask-filtered subscription by category (e.g., `EventMask::PIECE_COMPLETE | EventMask::PEER_BAN`)
   - Bounded queue, drop oldest or emit a synthetic "missed" event on overflow

2. **Piece extent affinity for disk locality**  
   - Track 4 MiB (or tunable) extents of contiguous pieces
   - When a peer starts on an extent, preferentially add blocks from that extent
   - Falls back to rarest-first if extent is exhausted
   - Significant win on slow spinning-disk setups

3. **Separate disk thread pool**  
   - Keep disk I/O off the tokio runtime entirely
   - Use a dedicated `tokio::task::spawn_blocking` pool or custom thread pool
   - Disk completion callbacks post back to the network reactor via `io_context` (or tokio channel)
   - Prevents latency spikes from page cache misses

4. **Sparse merkle tree representation**  
   - Don't require the full tree upfront; lazily load nodes as they're verified
   - Store only interior nodes + verified leaves; derive zeroed padding hashes
   - Allows magnet-link workflows where metadata (and tree) arrives later

5. **Per-file merkle trees for v2**  
   - Each file has its own merkle tree with a root hash in the metainfo
   - Hash picker fetches hashes per-file, not per-piece
   - Enables streaming recovery: don't need the full file tree to verify early pieces

---

## 7. What Magpie Should Avoid

1. **Unbounded job queues**  
   - Rasterbar's disk job queue is unbounded; heavy write spikes can OOM
   - magpie should set a hard limit (e.g., 1000 pending jobs) and either backpressure producers or drop low-priority work

2. **C++-specific overhead artifacts**  
   - No need for intrusive lists, heterogeneous queues, or double-buffering-for-performance tricks
   - Tokio's channels and bounded queues are fast enough; favor clarity

3. **Affinity as a first-class optimization for all peer classes**  
   - Rasterbar applies it universally; it helps on rotational media (the common case in 2010)
   - For modern NVMe, the overhead of tracking extents may exceed the benefit
   - Make it opt-in or dynamic based on detected I/O latency

4. **Hard-coded magic numbers**  
   - `max_piece_affinity_extent = 4 MiB` (line 146)
   - `2048` block queue cap (line 1994)
   - `5` peer disconnect threshold (line 3142)
   - All should be configurable in a settings struct

5. **Endgame mode as a peer-level flag**  
   - Rasterbar attaches `m_endgame_mode` to every peer connection; this couples torrent state to peer state
   - Better: track endgame as a torrent-level decision, feed it to the picker, and let the picker instruct peers what queue depth to use
   - Cleaner separation of concerns

---

## Codebase Navigation

| Component | File | Key Lines |
|-----------|------|-----------|
| Alert base & categories | `include/libtorrent/alert.hpp` | 76–185 (categories), 189–330 (base class) |
| Alert manager | `include/libtorrent/aux_/alert_manager.hpp` | 61–180 |
| Alert manager impl | `src/alert_manager.cpp` | 48–149 |
| Piece picker header | `include/libtorrent/piece_picker.hpp` | 105–240 |
| Piece picker impl | `src/piece_picker.cpp` | 1–162 (init), 2200–2260 (affinity), 3090–3195 (extent tracking) |
| Disk thread pool | `include/libtorrent/aux_/disk_io_thread_pool.hpp` | (no file found; split into mmap_disk_io.cpp) |
| Disk pool impl | `src/disk_io_thread_pool.cpp` | 48–210 |
| Disk I/O job queue | `src/mmap_disk_io.cpp` | 241–380 (structures), 715–838 (async ops) |
| Merkle tree | `include/libtorrent/aux_/merkle_tree.hpp` | 83–150 |
| Hash picker | `include/libtorrent/hash_picker.hpp` | 35–100, 150–213 |

---

## Summary

Libtorrent-rasterbar exemplifies **robust production BitTorrent** at scale. The alert system's non-blocking queue with category masking and the piece picker's extent affinity heuristic are high-value patterns. The disk I/O pool cleanly decouples storage from the network reactor.

However, the codebase reflects **2010s C++ trade-offs**: manual memory management, type erasure, intrusive data structures, and magic constants. Magpie should **borrow the algorithms and architecture** but **reimplement in idiomatic Rust**: enums over traits, channels over intrusive queues, and configurations over hard-coded limits.

The v2/BEP 52 support is solid: sparse merkle trees and per-file verification are well-designed. The abstraction cleanly separates v1 SHA1 hashing from v2 SHA256 per-block hashing.

