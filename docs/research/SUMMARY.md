# Research summary — cross-cutting distillation

**Date**: 2026-04-13. Reads across [001-cratetorrent](001-cratetorrent.md), [002-anacrolix-torrent](002-anacrolix-torrent.md), [003-libtorrent-rasterbar](003-libtorrent-rasterbar.md), [004-librqbit](004-librqbit.md), [005-monotorrent](005-monotorrent.md), [006-lambdaclass-libtorrent-rs](006-lambdaclass-libtorrent-rs.md).

## Convergent patterns (close to universal truth)

1. **Task-per-peer, channel-coordinated engine.** cratetorrent (§7), anacrolix (§4), librqbit (§1), rasterbar (disk pool), and MonoTorrent all keep peers independent with explicit boundaries. No one shares a giant mutex across peers.
2. **Pluggable storage with `ReadAt`/`WriteAt` semantics.** anacrolix (`PieceImpl`, §2), rasterbar (mmap-first with pluggable variants), MonoTorrent. cratetorrent and librqbit skip this; both regret it implicitly (hard to add mmap/sqlite later).
3. **Disk I/O off the network loop.** rasterbar's dedicated pool, cratetorrent's `block_in_place`, anacrolix's sync callbacks on separate paths. Universal.
4. **Resume data = piece bitfield on disk.** Every implementation persists bitfield state; librqbit's `.bitv` and MonoTorrent's `.fastresume` are the cleanest. Nobody persists *consumer* stats inside the library.
5. **Roaring bitmaps for bitfields** (anacrolix, rasterbar) once swarms grow. Worth adopting when we have >1 torrent and >few hundred pieces.
6. **Hardcoded peer-ID prefixes** in every implementation except MonoTorrent's compile-time-parameterised version. This is specifically magpie's differentiator.

## Divergent patterns (ADR-worthy choices)

1. **Event delivery**: broadcast (magpie proposal), unbounded mpsc alerts (cratetorrent), synchronous callbacks (anacrolix, MonoTorrent), double-buffered poll queue (rasterbar), no event bus (librqbit).
   - **Finding**: synchronous-callback model has known pain (anacrolix §8, MonoTorrent pain-points §6). Polling is librqbit's documented gap. broadcast gives us typed events + bounded backpressure without consumer-stall risk.
   - **Feeds**: ADR 0002.

2. **Storage abstraction shape**: trait with `ReadAt`/`WriteAt` returning per-piece handles (anacrolix), monolithic per-torrent struct (rasterbar), no abstraction (cratetorrent, librqbit).
   - **Finding**: anacrolix shape is the clear winner. Async-translated, it maps to a `Storage` trait returning a `TorrentStorage` handle; each `PieceHandle` implements `AsyncReadAt`/`AsyncWriteAt`. Streaming (M6) falls out naturally.
   - **Feeds**: new ADR candidate (storage trait shape).

3. **Piece picker data structure**: `Vec<Piece>` linear scan (cratetorrent), B-tree with availability+priority key (anacrolix), priority buckets + 4 MiB extent affinity (rasterbar).
   - **Finding**: start with anacrolix's B-tree (production-proven, O(log n)). Add rasterbar's speed-class affinity as a later picker upgrade (explicit M5 deliverable in ROADMAP).
   - **Feeds**: new ADR candidate (picker architecture).

4. **v1/v2 hash abstraction**: MonoTorrent's `IPieceHashes` interface + `PieceHashesV1`/`PieceHashesV2` impls is the cleanest (005, §2). rasterbar uses C++ templates on SHA1/SHA256. anacrolix uses separate modules but no unifying trait. cratetorrent and librqbit are v1-only. **lambdaclass/libtorrent-rs is v1-only too (despite being on the reading list as "v2 reference" — correction needed)**.
   - **Finding**: adopt MonoTorrent's shape, translated to Rust: `enum PieceHash { V1([u8;20]), V2([u8;32]) }` and `enum InfoHash { V1, V2, Hybrid { v1, v2 } }`.
   - **Feeds**: new ADR candidate (v1/v2 hash data model).

5. **Backpressure on disk writes**: rasterbar pool + queue, anacrolix sync reply-per-op, cratetorrent unbounded buffer (known bug, issue #22).
   - **Finding**: bounded disk queue with backpressure back to peers. Never accept blocks faster than we can flush.

## Corrections to PROJECT.md / ROADMAP.md

Research surfaced three factual problems in our own docs:

1. **`librqbit-utp` does not exist** in the current librqbit tree. The "Userspace uTP + metrics | librqbit-utp" line in PROJECT.md's inspiration table is wrong. librqbit relies on TCP; its transport abstraction (`StreamConnector`) handles TCP+SOCKS only. Our M4 uTP will reference rakshasa + rasterbar only. **Fix**: edit PROJECT.md inspiration table and `docs/ROADMAP.md` reading-order note.
2. **lambdaclass/libtorrent-rs is v1-only**, not a v2 reference (the project README itself states "only V1 is implemented but we're working on V2"). It remains useful for Rust bencode/metainfo shape but not for v2. **Fix**: adjust ROADMAP.md reading-order note and the ADR 0003 cite.
3. **librqbit's "Arc<Mutex<Session>> god-object" claim** applies to librqbit 8 as lightorrent encountered it, not the current tree. Current librqbit uses `Arc<Session>` + `RwLock<SessionDatabase>` + per-torrent Arcs. **Fix**: nuance `docs/archive/librqbit-gap-analysis.md` (keep as historical; don't apply to current librqbit).

## Recommendations per subsystem

| Subsystem | Recommendation | Primary source |
|---|---|---|
| Piece picker | B-tree with `(availability, priority, partial)` key, rarest-first baseline, endgame at `free_count == 0`. Speed-class affinity deferred to M5. | anacrolix §3, rasterbar §2, cratetorrent §2 |
| Storage | Trait: `Storage → TorrentStorage → PieceHandle`; `PieceHandle: AsyncReadAt + AsyncWriteAt`. File backend uses `pwritev`/`preadv` (Linux) + portable fallback (macOS/Windows). In-memory backend for tests. Bounded write queue with backpressure. | anacrolix §2, cratetorrent §3, rasterbar §3 |
| Event bus | Custom rasterbar-style alert ring: double-buffered arena + generation swap + category mask + batch drain. Zero heap alloc per event in steady state. Single primary reader; consumer-side fan-out helper for sidecars. See [ADR 0002](../adr/0002-event-bus-alert-ring.md). | rasterbar §1; project perf preference |
| Hash data model | `enum PieceHash { V1([u8;20]), V2([u8;32]) }`, `enum InfoHash { V1, V2, Hybrid { v1, v2 } }`, merkle helpers isolated in their own module. | MonoTorrent §2, rasterbar v2 §4, anacrolix merkle §6 |
| Peer-ID builder | `PeerIdBuilder { client_code: [u8;2], version: [u8;4] }` → Azureus-style `-CCVVVV-<random>`, documented randomness source. | librqbit gap §6; MonoTorrent pattern (minus hardcoding) |
| Concurrency shape | Per-torrent task owns picker + in-progress piece state. Peers talk to it via mpsc. Session `Arc<Session>` holds global state (DHT, rate limits). | librqbit §1, cratetorrent §7 (but without cratetorrent's nested `RwLock<_, RwLock<_>>`) |
| Resume data | Persist only `.bitv` bitfield + minimal metadata. Consumer state (stats, ratios) is consumer-owned. | librqbit §3, §5; MonoTorrent pattern |
| v2 merkle verification | Sparse tree with lazy node loading; per-file merkle roots. `HashPicker` manages verification state per file when metainfo omits layers. | rasterbar §4, anacrolix §6, MonoTorrent §2 |

## ADR feedback

### ADR 0001 — Subcrate vs. feature for DHT and uTP

**Direction**: Subcrates.

Research rationale: rasterbar and anacrolix both keep DHT and uTP functionally isolated from the core torrent engine. rasterbar specifically separates the disk/net/DHT threading domains. librqbit's current tree lacks uTP entirely — which underlines the need for clean isolation (we can compose, but only if boundaries are strict). Compile-time isolation via subcrates (`magpie-bt-dht`, `magpie-bt-utp`) prevents accidental cross-cutting coupling and keeps small builds small.

### ADR 0002 — Event bus: custom rasterbar-style alert ring

**Direction**: Accept the custom ring. Build a double-buffered, arena-backed, category-masked alert queue under `magpie-bt-core/src/alerts/`. High-frequency per-block/per-request streams use dedicated bounded `mpsc` channels, not the alert ring.

Research rationale:
- rasterbar's ring is the reference design for sustained-throughput BT event delivery at gigabit scale (003 §1). Magpie is a library; perf regressions are felt by every consumer.
- librqbit's polling is the gap magpie must close (004 §2).
- anacrolix's sync callbacks under locks is a documented failure mode (002 §4, §8).
- `tokio::sync::broadcast` was considered and rejected: per-subscriber deep clones scale poorly at 8000+ events/s, and `broadcast<Arc<T>>` still pays a heap alloc per event with no batch drain.

Consequence: overflow policy is explicit — `Alert::Dropped(n)` sentinel enqueued when consumer falls behind; producer never blocks. Consumers that retain alerts past a generation swap must copy them out (enforced by lifetimes).

### ADR 0003 — Tokio-only runtime

**Direction**: Accept.

Research rationale: cratetorrent and librqbit are both tokio-only and this has not been a pain point in either codebase. anacrolix is Go, rasterbar is C++ — neither has a runtime choice to make. Our only consumer (lightorrent) is tokio-based. Reconsider only if a concrete benchmark against a single-runtime design shows daylight.

## Candidate new ADRs (to open during M0)

1. **Storage trait shape** — adopt anacrolix-style `Storage → TorrentStorage → PieceHandle`, async translation, Rust-native traits. Needed before the M0 file-backed impl lands.
2. **Piece-picker architecture** — B-tree with `(availability, priority, partial)` sort key; later upgrade to speed-class affinity. Needed before M1.
3. **v1/v2 hash data model** — `PieceHash` and `InfoHash` enums, merkle helper module. Needed from M0 because v2 invariants (16 KiB blocks, power-of-two piece sizes) must be enforced even when only v1 is supported on the wire.
4. **Disk write backpressure** — bounded disk queue with explicit signal back to peers; caps the cratetorrent issue-22 footgun. Needed before M2 (upload side).

Total ADR count expected by M0 close: 7 (0001–0003 accepted + 0004 storage + 0005 picker + 0006 hash model + 0007 disk backpressure).
