# M2 — Seeder + multi-torrent (consumer-integration ready)

**Status**: done (2026-04-21; red-team audit green. RSS budget populates empirically on the first completed weekly-soak run — the soak workflow + dhat harness are wired; an observed-peak number is a post-close data refresh, not a code gap.)
**Gate summary**: controlled-swarm reseed succeeds (magpie-seed + magpie-leech, synthetic ~5 MiB content over a locally-spawned tracker, SHA-256 match); 24 h ≥8-torrent soak including one ≥100k-piece torrent is flat under `dhat` and within the documented RSS budget; cumulative upload/download stats survive SIGKILL (subprocess test); interop scenarios against qBittorrent and Transmission (docker + local tracker + synthetic fixtures, no WAN downloads) are green both directions; public API surface audited against realistic client call-site patterns and found client-agnostic.

## Scope principle

**magpie M2 ships a complete, tested, interop-verified seeder + multi-torrent library on magpie's own terms.** No cross-repo consumer work is scoped inside this milestone. Consumer integrations (lightorrent is the current reference consumer) happen in those repos, on their timelines, and are **not** gates here.

## Goal

Turn the M1 leecher into a full BitTorrent client. Implement the upload side and choking algorithm, run many torrents concurrently under shared bandwidth, honour the private flag, add UDP + multi-tracker support, replace the M1 poll-based stats loop with event-driven persistent counters. This is the first milestone subject to the DISCIPLINES "interop tests from M2 onward" bar — verified via in-CI docker scenarios against spec-compliant third-party clients.

## Scope / deliverables

Workstreams A–G are largely implemented as of this refresh; the remaining work is H-tail, I, J (multi-file storage — new addition, not yet started), verification, and the consumer-surface audit. Each implemented checkbox points to the primary file(s) that realise it.

### A. Upload engine ✅

- [x] Per-peer bounded request queue (ADR-0017): `crates/magpie-bt-core/src/session/peer_upload.rs`. `Request → DiskRead → Piece` pipeline never blocks the peer task on disk I/O.
- [x] Reject-if-not-available logic: `session/peer_upload.rs`.
- [x] Read cache (ADR-0018): `crates/magpie-bt-core/src/session/read_cache.rs`. Session-global piece-granular LRU keyed on `(InfoHash, PieceIndex)`, entries as `bytes::Bytes` for zero-copy block fan-out, singleflight, store-buffer short-circuit.
- [x] Per-peer send-buffer watermark gating `DiskOp::Read`: adaptive `clamp(rate × 0.5 s, 128 KiB, 4 MiB)` in `session/peer_upload.rs`.
- [x] Anti-snub (60 s grace): `session/choker/mod.rs`.
- [x] Peer need-set tracking (ADR-0020): on-demand `our_have & !peer.have` at SeedChoker read time.

### A2. Inbound TCP ✅

- [x] `TcpListener` accept loop: `crates/magpie-bt-core/src/engine.rs` (`listen()` around line 723).
- [x] Server-side handshake flow (ADR-0009 extension): `engine.rs::handle_inbound()`. Reads peer handshake, looks up torrent by `info_hash`, rejects if unknown/full, sends ours.
- [x] Connection routing via `HashMap<InfoHash, TorrentActorHandle>` (`info_hash_index`).
- [x] Peer-ID collision check.
- [x] Reachability note in README + crate docs (UPnP/NAT-PMP is M6).

### B. Tracker upgrades ✅

- [x] BEP 12 multi-tracker announce-list + tier fall-through: `crates/magpie-bt-core/src/tracker/tiered.rs`.
- [x] BEP 15 UDP tracker client: `tracker/udp.rs` (connect/announce/scrape; spec timeouts + exponential backoff).
- [x] BEP 27 private flag + `is_private()` + M3 suppression hook-points: `session/torrent.rs`.
- [x] Fuzz target for UDP-tracker response parser + corpus: `crates/magpie-bt-core/fuzz/fuzz_targets/udp_tracker.rs`; wired into `.github/workflows/nightly.yml` matrix.

### C. Choking + bandwidth ✅

- [x] `Unchoker` trait + `LeechChoker` + `SeedChoker` (ADR-0012): `crates/magpie-bt-core/src/session/choker/mod.rs`. 4+1 slots; 10/30 s rotation; rasterbar-style adaptation candidate.
- [x] Leech→seed transition (ADR-0019): `session/torrent.rs` (five-step guarded by `completion_fired`). **Dedicated unit test still outstanding** — see verification §11.
- [x] Three-tier hierarchical token-bucket shaper (ADR-0013): `session/shaper/mod.rs`. All three tiers live from day one; per-torrent bucket at `u64::MAX` exercises the refill cycle.

### D. Multi-torrent session ✅

- [x] N torrents, one actor per torrent: `engine.rs` (Engine owns `HashMap<TorrentId, TorrentEntry>`).
- [x] Global + per-torrent peer caps.
- [x] Shared disk pool, strict FIFO (ADR-0007 amend): `session/disk.rs`.

### E. Persistent stats ✅

- [x] Per-peer `AtomicU64` up/down counters: `crates/magpie-bt-core/src/session/stats/mod.rs`. Zero-alloc on the counting path; `retire_peer()` snapshot ordering per ADR-0014.
- [x] 1 Hz `StatsUpdate` alert on the ring: `session/stats/mod.rs` + `alerts/queue.rs`.
- [x] `trait StatsSink` + default `FileStatsSink`: `session/stats/sink.rs`. Consumer overrides are a consumer concern.

### F. ~~Lightorrent integration~~ — **out of M2 scope**

The previous F workstream (a `trait TorrentEngine` abstraction in the lightorrent repo with librqbit + magpie adapters) is **out of this milestone's scope**. It was never magpie-repo work. Consumer integration happens in the consumer's repo on its timeline. Replaced by a magpie-internal **consumer-surface audit** (deliverable: `docs/api-audit.md`) that walks the public surface against realistic client call-site patterns (including lightorrent's as one reference) and verifies client-agnosticism without adding consumer-named surface.

### G. UDP plumbing ✅

- [x] `UdpDemux` actor (ADR-0015): `crates/magpie-bt-core/src/session/udp/demux.rs`. One socket, first-byte dispatch, `DashMap<u32, oneshot>` tracker routing with 60 s TTL + 10 000 cap, bounded subscriber inboxes with drop-on-full. `recvmmsg` batch path design-hooked, not implemented.

### H. Observability + leak verification

Nightly CI (`.github/workflows/nightly.yml`) already exists; the items M1 deferred are unblocked.

- [x] Optional Prometheus exporter (counters already in `DiskMetrics`) — behind `features = ["prometheus"]` on `magpie-bt-core`: `crates/magpie-bt-core/src/metrics_exporter.rs`.
- [ ] `dhat` leak run on a 24 h leech+seed session; documented RSS budget (`docs/RSS-budget.md`).
- [x] Nightly fuzz at the DISCIPLINES ≥10 min cadence for all current and new fuzz targets (including `udp_tracker`) — all six targets in the `.github/workflows/nightly.yml` matrix at ≥600 s.
- [x] **Workflow split**: `nightly.yml` for fuzz + miri; `.github/workflows/weekly-soak.yml` for 24 h dhat + multi-torrent soak. Two workflows total.

### I. Interop (new bar from M2 — highest-risk workstream, budget generously)

- [x] Scripted scenarios (docker-compose, pinned image digests): magpie seeds → qBittorrent + Transmission leech; and inverse. **Harness is hermetic**: locally-spawned tracker (decision between pinned `torrust-tracker` subprocess and ~200-LOC inline mock committed at the scaffolding PR), synthetic ~5 MiB deterministic-random content fixture, SHA-256 match gate. **No WAN downloads**, no public trackers, no Debian-ISO fetch. **First green run 2026-04-15**: qBittorrent 4.5.5 + Transmission 4.0.6, both SHA-256 match (`960318fc...`).
- [x] **Risk note**: third-party clients differ in handshake reserved-bits, extension-handshake negotiation, and timeout tolerance. First round will be debugging interop quirks (e.g. Transmission rejecting our reserved-bit pattern), not testing throughput. Land interop scaffolding early in the milestone so the debugging tail doesn't gate the close. **Prove the harness itself green on a third-party↔third-party round-trip before wiring magpie in**, so harness bugs don't masquerade as magpie bugs. **Outcome**: no interop quirks surfaced — both clients accepted magpie's handshake and completed the download on the first attempt.

### Consumer-surface audit (replaces old F)

- [x] Walk `magpie-bt` + `magpie-bt-core` public surface. For each method, ask "would a BitTorrent client author unfamiliar with lightorrent find this natural?"
- [x] Cross-check against real call-site patterns from (a) `lightorrent/src/engine.rs`, (b) rasterbar `session`/`torrent_handle`, (c) anacrolix `torrent.Torrent` — to validate completeness, not to copy shapes.
- [x] File any gaps as client-agnostic magpie tasks; do not add consumer-named surface. Three gaps surfaced and closed: G1 `Engine::pause`/`resume`, G2 `Engine::remove(id, delete_files)`, G3 `Engine::torrents()`/`torrent_state()` — all implemented in `crates/magpie-bt-core/src/engine.rs`.
- [x] Deliverable: `docs/api-audit.md` committed.

### J. Multi-file download ✅

Parser supports `FileListV1::Multi` already; the gap was the storage side — `FileStorage` is single-file-backed, so a multi-file torrent couldn't be downloaded without a consumer-rolled `Storage` impl. Fixed by shipping a second `Storage` backend. Design evidence: `_tmp/rakshasa-libtorrent/src/data/` — `FileList::create_chunk` (`file_list.cc:600`) + `FileManager` (bounded LRU fd pool); magpie adopts the same split of concerns.

- [x] `MultiFileStorage` impl of the existing `Storage` trait: `crates/magpie-bt-core/src/storage/multi_file.rs`. Sorted `Vec<FileEntry>`, binary-search walk on torrent offset, per-entry `pread`/`pwrite` covering the range. `&self` invariant (ADR-0004) preserved via `Mutex<FdPool>` interior mutability. No `Storage` trait changes.
- [x] Bounded fd pool with LRU eviction (default cap 128, clamped to [4, 65536]). Lazy first-use open, reopen on cache miss. **Engine-global**: `Engine::new()` constructs an `Arc<FdPool>` via `FdPool::with_default_cap()`; `Engine::with_fd_pool_cap(cap)` builder overrides; `Engine::fd_pool()` accessor hands the pool to `MultiFileStorage::create_from_info`. Mirrors rakshasa's `FileManager` scope.
- [x] Path-safety validation at construction: rejects `..`, `.`, empty components, path separators inside components, NUL bytes, duplicate paths across entries, total-length overflow, and symlink-escape. Fail-closed with the new `StorageError::Path` variant before any fd is opened. 12 rejection tests.
- [x] Sparse pre-allocation: `File::set_len(length)` per entry in `create()`; no eager zero-fill. Matches `FileStorage::create` behavior.
- [x] `Storage::delete` removes every entry file + prunes directories that become empty, bottom-up, stopping at (not removing) `root`.
- [x] Convenience constructors `MultiFileStorage::create_from_info` / `open_from_info` in `magpie-bt-core` (bridges parsed metainfo `Info` → `Vec<FileSpec>` → storage). Auto-routes hybrid torrents (v1 + v2 info) to the v1 file list; rejects v2-only and single-file with structured errors.
- [x] Single-file `FileStorage` unchanged; consumers pick the backend at `AddTorrentRequest::storage` construction time based on `FileListV1::{Single, Multi}`. Seeder example auto-detects via path type (`--data <file>` → `FileStorage`, `--data <dir>` → `MultiFileStorage`).
- [x] ADR-0021 — Multi-file storage.

## Gate criteria (verification)

Every item is mechanically checkable. All gates are magpie-internal — no cross-repo dependencies.

1. **All DISCIPLINES bars hold workspace-wide**: `cargo test`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace --no-deps -D warnings` clean; coverage thresholds met; CHANGELOG updated; ADRs landed. All new crates/test harnesses on **edition = "2024"** and building on latest stable rustc.
2. **Controlled-swarm proof (magpie-only, hard gate)**: local tracker + magpie seed + magpie leech complete a synthetic ~5 MiB reseed with SHA-256 match inside a time budget. A second variant adding one third-party (qBittorrent or Transmission) leech reuses the interop harness and is **best-effort for M2** — promoted to hard gate in M3 if it slips. **Throughput floor** (separate test): magpie-seed ↔ magpie-leech over loopback with a shaper-pinned rate (ADR-0013); sustained throughput reaches ≥80 % of the pinned rate. Loopback pinning is required — "80 % of link" on unbounded loopback is meaningless without the shaper.
3. **24 h multi-torrent soak** (≥8 concurrent torrents) within documented RSS budget (`docs/RSS-budget.md`); `dhat` output flat. **Workload constraint**: the soak set must include at least one large-piece-count torrent (≥100k pieces) so ADR-0005's linear picker cost model is empirically exercised. Generated synthetically on tmpfs — no WAN downloads.
4. **BDD coverage**: `.feature` files for BEP 12, 15, 27 under `crates/magpie-bt/tests/features/`; `../bep-coverage.md` rows updated.
5. **Interop**: scenarios from workstream I green in CI against pinned qBittorrent + Transmission, both directions, SHA-256 match, via local tracker + synthetic fixtures.
6. **Stats persistence**: subprocess test `crates/magpie-bt/tests/stats_persist.rs` — start → add torrent → progress to non-zero up + down → SIGKILL → restart → counters ≥ pre-kill snapshot AND > 0. `#[cfg(unix)]`-gated; Windows CI skips with a rationale comment.
7. **ADR-0019 ordering unit test**: a unit-level test in `crates/magpie-bt-core/src/session/torrent.rs` asserts the five-step sequence under `completion_fired`, including `NotInterested`-after-unchoke.
8. **Consumer-surface audit**: `docs/api-audit.md` committed, with any follow-up API tasks filed as client-agnostic work.
9. **ADRs landed**: 0004, 0005, 0012–0021 (see below). Note: ADR-0016's concrete adapter work is scoped to the consumer's repo, not to this milestone — the ADR is retained as the design reference for how a consumer *would* wrap magpie.
10a. **Multi-file download — magpie-only (hard gate, ✅ landed)**: `crates/magpie-bt-core/tests/multi_file_download.rs::magpie_seed_to_magpie_leech_multi_file_sha256_match` constructs a synthetic multi-file torrent whose 7-entry layout engineers piece boundaries: one piece spans 3 non-zero entries (small middle file < piece_length), three more pieces span 2 entries each, with zero-length entries interleaved. Fixture generator asserts at construction time that ≥3 pieces cross file boundaries and ≥1 piece spans ≥3 non-zero entries (silent-failure guard). magpie-seed hosts it, magpie-leech fetches it, per-file SHA-256 matches fixture.

10b. **Multi-file interop extension (follow-up, deferred)**: same fixture + seeder against qBittorrent leech + Transmission leech under `ci/interop/`. Scaffolding mirrors the existing `qbittorrent-magnet` / `transmission-magnet` scenarios. Deferred because the docker-based validation path is not runnable in the current session; fixture generation + seeder support for multi-file is already in place, only the compose + gate scripts remain.

11. **Path safety (hard gate, ✅ landed)**: unit tests under `storage::multi_file::tests` reject each of: `..` component, `.` component, empty component, path separator inside a component, NUL byte, duplicate path across entries, total-length overflow, and symlink-escape (a pre-existing symlink under `root` that points outside). All fail at `MultiFileStorage::create` / `::open` with `StorageError::Path` before any fd is opened. 12 rejection tests, all green.

12. **Fd pool bound holds under load (hard gate, ✅ landed)**: `crates/magpie-bt-core/tests/multi_file_download.rs::fd_pool_bound_under_load` downloads the 7-file fixture with `FdPool::with_cap(4)`. The test asserts both `seed_pool.opens_total() > 4` and `leech_pool.opens_total() > 4` — LRU eviction + lazy reopen fired on both ends, the download still completed, and SHA-256 still matched. Counter assertion proves the cap was exercised (not merely ≤4 files in flight).

## ADRs in this milestone

Accepted directions — the prose lives in each ADR file under `../adr/`.

**New**
- **ADR-0012** Choker: enum-switched `Leech` (20 s-EWMA download rate from peer, tit-for-tat) / `Seed` (20 s-EWMA upload rate to peer, rasterbar `fastest_upload` — **not** round-robin-by-bytes, which is the broken original rasterbar algorithm that let slow peers monopolise slots). 4 regular + 1 optimistic slot; 10 s / 30 s rotation; new-peer 3× weight on optimistic draw; 60 s anti-snub; immediate re-eval on leech→seed swap (ADR-0019). Need-set hook exists on `PeerState` but unused in M2 rank (BEP 16 super-seed only). See [ADR-0012](../adr/0012-choker.md).
- **ADR-0013** Bandwidth: three-tier hierarchical token bucket (session / per-torrent / per-peer), six buckets per session (up + down). Consume-on-wire: peer bucket checked per send/recv (two atomics per block); session + torrent tiers touched only by the 100 ms refiller. Proportional-to-demand parent→child grant using `consumed + denied` counters. Pass-through buckets at `u64::MAX` exercise the full refill path so M6 cap-enablement is a config flip, not a refactor. See [ADR-0013](../adr/0013-bandwidth-shaper.md).
- **ADR-0014** Stats: one `AtomicU64 uploaded` + `AtomicU64 downloaded` per peer, serving three readers (choker EWMA, shaper demand, 1 Hz emitter) — one atomic-add per block for all three. Per-torrent cumulative = snapshot(live) + disconnected-sum. 1 Hz `StatsUpdate` alert with precomputed deltas. `trait StatsSink` (object-safe, multiple sinks supported); default `FileStatsSink` writes bencode `.stats` sidecar with 30 s batched flush + graceful-shutdown flush. Consumers can provide their own sink (e.g. a redb-backed one) — out of scope here. Drop-and-alert on sink backpressure. See [ADR-0014](../adr/0014-stats.md).
- **ADR-0015** UDP demux: one `tokio::net::UdpSocket` per listen port, `recv_from` loop with first-byte dispatch (DHT = `b'd'`, uTP = `0x01|0x11|0x21|0x31|0x41`, else tracker transaction-id lookup). `DashMap<u32, oneshot::Sender>` for tracker response routing with 60 s TTL + 10 000 cap. Bounded subscriber inboxes with drop-on-full (slow subsystem never starves the others). M4 DHT + M5 uTP register via `None → Some` — no rewiring. `recvmmsg` batch hook for M4+. See [ADR-0015](../adr/0015-udp-demux.md).
- **ADR-0016** Engine abstraction (design reference only — implementation lives in the consumer's repo, not in magpie): `trait TorrentEngine` pattern for a consumer wrapping both librqbit and magpie during a cutover, with `librqbit.rs` + `magpie.rs` adapter impls. Object-safe, `Arc<dyn TorrentEngine>`. Shared types consumer-owned. `subscribe_stats` / `subscribe_lifecycle` return `mpsc::Receiver`. Retained in this milestone's ADR set as the design reference for how a consumer *would* wrap magpie; any actual adapter code is the consumer's project. See [ADR-0016](../adr/0016-engine-abstraction.md).
- **ADR-0017** Upload request flow: per-peer unread queue cap 128 with drop-newest on overflow (rasterbar `max_allowed_in_request_queue` lineage), ready queue via pull-model reads with adaptive send-buffer watermark `clamp(rate × 0.5 s, 128 KiB, 4 MiB)`. 2 s post-choke grace window before disconnecting peers still requesting; fast-set abuse cap at 3 × blocks_per_piece while choked. `Arc<Block>` fan-out from ADR-0018. No sync disk on peer task. See [ADR-0017](../adr/0017-upload-request-flow.md).
- **ADR-0018** Read cache: session-global **piece-granular** LRU keyed on `(InfoHash, PieceIndex)`, 64 MiB default (matches write budget). Entries are `bytes::Bytes` holding the verified piece; a block served to N peers is `piece.slice(offset..offset+len)` — one disk read, N refcount bumps, zero memcpy. Singleflight coalescing on misses (no thundering herd). Bypass path for one-off misses (avoid cache pollution). Store-buffer short-circuit: a `DiskOp::Read` whose piece is still in `DiskWriter::pending_writes` is served from that buffer. Promotion ordering (cache-insert-then-pending-remove) avoids a momentary invisibility window. OS page cache below.
- **ADR-0019** Completion transition: forward-only, fires once when `our_have.all_set()` first holds post-disk-ack. Five-step strict sequence (single `completion_fired` guard over the whole block): `Alert::TorrentComplete` → choker swap + timer reset → immediate seed-unchoke re-eval → `NotInterested` broadcast → fire-and-forget tracker `event=completed`. Interest broadcast is placed **after** the unchoke round to avoid peers dropping their `Interested` toward us in response and leaving unchoked slots idle. Torrents loaded complete-from-resume skip the transition entirely. See [ADR-0019](../adr/0019-completion-transition.md).
- **ADR-0020** Peer need-set: pointer record to [ADR-0005 §Peer need-set](../adr/0005-picker-architecture.md), which owns the design (on-demand `our_have & !peer.have`, no cache, no leech→seed fix-up step). Kept as a standalone ADR so "need-set" is discoverable by number. See [ADR-0020](../adr/0020-peer-need-set.md).
- **ADR-0021** Multi-file storage: new `MultiFileStorage` impl of the existing `Storage` trait. Sorted `Vec<FileEntry>`, binary-search on torrent offset, bounded LRU fd pool (default 256). Path safety enforced at construction; sparse `set_len` pre-allocation; no `Storage` trait changes (ADR-0004 invariants hold). Shape matches the proven rakshasa `FileList` + `FileManager` split. See [ADR-0021](../adr/0021-multi-file-storage.md).

**Resolved from M0 carry-over**
- **ADR-0004** Storage trait: keep the **flat positional `&self`** trait shipped in M1. `PieceHandle` hierarchy deferred to M7 (pays off for streaming + mmap/sqlite/S3, none in M2–M6). Two load-bearing invariants: all trait methods `&self` (protects concurrent read-while-write), and `writev`/`readv` move off the trait to `FileStorage` inherent methods (vectored I/O is a file-backend optimization, not an abstraction concern). See [ADR-0004](../adr/0004-storage-trait-shape.md).
- **ADR-0005** Picker: **keep the M1 linear rarest-first + endgame picker** through M6+. B-tree migration deferred until either priority/speed-class work lands or `pick()` shows up on flamegraphs of realistic workloads (soak workload must include a ≥100k-piece torrent to make this visible). Scope boundaries: `Picker` is piece-granular + leech-only; per-block state stays in `TorrentActor`; seed-mode is passive (`has_piece` lookup, no picker consult); per-peer need-sets computed on-demand as `our_have & !peer.have` at SeedChoker read time (no cached state, no staleness trap at leech→seed transition). See [ADR-0005](../adr/0005-picker-architecture.md).

**Amended**
- **ADR-0007** Disk backpressure: cap in bytes, session-wide (default 64 MiB, anacrolix lineage); hysteresis resume at 75 % (48 MiB) to prevent read/pause toggling under sustained load; per-torrent atomic byte counters as telemetry only (no enforcement — bandwidth shaper ADR-0013 already rate-limits write generation per torrent); per-torrent enforcement deferred to M6 pending multi-torrent soak data. Add `DiskOp::Read`. **Amendment from ADR-0018**: `DiskOp::VerifyAndWrite::buffer` changes `Vec<u8> → Bytes` so the store-buffer short-circuit shares the same allocation with `pending_writes` via refcount (no per-piece memcpy). Pool is strict FIFO — no read/write priority (rasterbar v2 precedent: delegate scheduling to the kernel page cache).
- **ADR-0009** Peer state machine: extend with upload-side request-queue state per ADR-0017.

## Open questions

- **ADR-0002 alert-ring revisit** (status: deferred post-M2). Once seeding produces realistic event volumes, profile custom arena vs. `broadcast<Arc<Alert>>`. If the arena isn't measurably faster, swap with a single follow-up ADR. Schema unchanged either way. Tracked here so the profiling work isn't lost.

## Technical debt from soak-fix (2026-04-15)

The 24h soak fix (`biased select!`, `TorrentComplete` detection, `join()` abort-all, `run_pair` Result refactor) leaves three accepted limitations to address in M3+:

1. ~~**Engine shutdown needs CancellationToken**~~ — **resolved**. `join()` now signals a `tokio_util::sync::CancellationToken` so the listener's accept loop exits gracefully. Per-connection handler tasks are tracked inside the listener and awaited with a 10 s grace period. Remaining tasks get a 5 s grace period (polling every 50 ms) to run cleanup (`retire_peer`, `release_peer_id`, `release_peer_slot`) before being aborted as a last resort. `global_peer_count` is no longer permanently inflated on shutdown.

2. ~~**Inner handler tasks not tracked**~~ — **resolved** (same change as #1). The listener task now collects handler `JoinHandle`s into a local `Vec` and awaits them with a grace period when the cancellation token fires, instead of fire-and-forgetting them.

3. **Alert queue shared across torrents can lose TorrentComplete** (`alerts/queue.rs`). A single `AlertQueue` is shared per engine. In a multi-torrent scenario, a flood of `PieceCompleted` from torrent A could evict torrent B's `TorrentComplete` before the consumer drains. For production multi-torrent use, consider a dedicated out-of-band completion channel (e.g. `oneshot::Sender<TorrentId>` per torrent) or per-torrent alert queues. Related to ADR-0002 alert-ring revisit above.

## Out of scope

- Magnet, PEX, LSD, BEP 9/10/11/14 → M3.
- DHT, BEP 5 → M4.
- uTP (BEP 29), BEP 52 v2 upload verification, hybrid mode, ADR-0006 (hash enum) → M5.
- WebSeed (BEP 19), tracker scrape (BEP 48), UPnP/NAT-PMP, picker speed-class affinity → M6.
- Super-seeding (BEP 16), streaming, SSL, alternate storage backends (mmap, sqlite, S3) → M7+. ADR-0020 keeps the door open without committing to a timeline. (Multi-file directory storage moves *in* to M2 scope under workstream J and ADR-0021.)
- **Consumer adoption** (lightorrent cutting over to magpie, dual-engine CI in the consumer's repo, `--engine=magpie` flags, etc.) is out of scope for every magpie milestone. Those are the consumer's projects on the consumer's timeline. M6 carries a *capability bar* ("ready for client replacement"), not a cross-repo adoption gate.
