# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (Parity Track A — resume-state persistence, ADR-0022)

- `ResumeSink` trait + `FileResumeSink` default impl under
  `crates/magpie-bt-core/src/session/resume.rs`. Mirrors the
  `StatsSink` pattern: `enqueue` is O(1) no-I/O, `flush_graceful` is
  a bounded-timeout write, sidecars are atomic write-to-tmp + rename.
  One bencode sidecar per torrent at `<dir>/<hex_info_hash>.resume`.
- Sidecar schema v1 (7 fields: `bitfield`, `info_hash`, `piece_count`,
  `piece_length`, `total_length`, `version`). Bitfield packed MSB-first,
  matching BEP 3 wire format. Forward-compat escape via
  `UnsupportedVersion` error for readers seeing a higher version.
- `Engine::torrent_bitfield_snapshot(id)` accessor — consumers poll this
  (on a timer or after `Alert::PieceCompleted`) to build a
  `ResumeSnapshot` and feed their `ResumeSink`. Library does not own
  write cadence, consistent with the stats-sink contract.
- `Picker::have_snapshot()` public helper — clones the verified-piece
  bitfield for resume-state persistence.
- `SessionCommand::BitfieldSnapshot { reply }` variant carrying the
  oneshot reply channel.
- 20 unit tests (pack/unpack boundary + short-buffer + trailing-bit
  tolerance, encode/decode roundtrip, schema rejection for missing
  fields / wrong version / bad info_hash length, sidecar file
  roundtrip, deduplication on info_hash, bounded graceful-flush,
  atomic-write preserves prior on overwrite).
- Integration test `crates/magpie-bt-core/tests/resume_roundtrip.rs`:
  seed hosts fixture, first leech downloads-then-pauses with seed
  up-rate pinned at 16 KiB/s so we actually catch a partial state,
  persists sidecar, shuts down. Second leech loads sidecar with
  `initial_have`, completes only the remaining pieces, SHA-256 match.
  Two silent-failure guards: first leech must be partially complete
  (not 0, not all); resume leech must emit exactly the remaining-piece
  count of `PieceCompleted` alerts (if it re-downloads everything,
  the assertion fails).
- Re-exports from `magpie-bt`: `FileResumeSink`, `ResumeSink`,
  `ResumeSinkError`, `ResumeSnapshot`.

### Added (M2 workstream J — multi-file download)

- `MultiFileStorage` (Unix): a new `Storage` impl backing a torrent onto a directory of files. Sorted entries + binary-search walk over torrent offsets + per-entry `pread`/`pwrite`. Zero-length entries are transparent. Writes that straddle file boundaries split across files atomically at the verified-piece granularity (existing `DiskWriter::VerifyAndWrite` semantics). See ADR-0021.
- `FdPool`: bounded LRU file-descriptor cache, timestamp-based eviction (O(n) scan per evict), no pinning. Default cap `128` (matches rakshasa's tier for Linux `ulimit -n 1024`); `FdPool::with_cap` clamps to `[4, 65536]`. Engine-global: `Engine::new` owns an `Arc<FdPool>`, `Engine::with_fd_pool_cap(cap)` overrides, `Engine::fd_pool()` accessor hands it into `MultiFileStorage` via the `*_from_info` bridges.
- Convenience bridges `MultiFileStorage::create_from_info` + `open_from_info` take a parsed `magpie_bt_metainfo::Info` and auto-route to the v1 file list (works for pure-v1 and hybrid torrents). Rejects v2-only and single-file with structured errors.
- `StorageError::Path` variant for fail-closed validation at construction. Rejects `..`, `.`, empty components, path separators inside components, NUL bytes, duplicate paths, total-length overflow, and symlink-escape. No partial state on disk if construction fails.
- Seeder example (`magpie-bt/examples/seeder.rs`) auto-detects single- vs multi-file from the `--data` path type: file → `FileStorage`, directory → `MultiFileStorage::open_from_info`. No flag needed.
- `crates/magpie-bt-core/tests/multi_file_download.rs` — two gates:
  - `magpie_seed_to_magpie_leech_multi_file_sha256_match` (gate 10a): 7-entry fixture engineered for boundary crossings (one piece spans 3 non-zero entries, three more span 2 entries each, zero-length entries interleaved). Fixture builder asserts at construction time that ≥3 pieces cross boundaries and ≥1 piece spans ≥3 non-zero entries (silent-failure guard). Per-file SHA-256 verified.
  - `fd_pool_bound_under_load` (gate 12): downloads the same 7-file fixture with `FdPool::with_cap(4)` on both ends. Asserts `opens_total() > 4` on both seed and leech to prove LRU eviction + lazy reopen fired, and that content still SHA-256-matches.

### Changed (M3 — BEP 10 extension-parser hardening)

- `magpie-bt-wire::extension` rewritten with a proper `ExtensionError` enum (5 structured variants: `Decode`, `InvalidExtensionId { name, id }`, `TooManyExtensions(usize)`, `MetadataSizeTooLarge(u64)`, `NonUtf8ExtensionName`) replacing the prior `Result<Self, &'static str>`. `MAX_EXTENSIONS = 128` bound exposed; reuses `MAX_METADATA_SIZE` from `metadata.rs`. `PartialEq`/`Eq` derived on `ExtensionHandshake` + `ExtensionRegistry`; `ExtensionRegistry` gains `Default`. 22 tests (up from 3), including negative/boundary cases (ext-id too large, non-utf8 name, metadata_size over limit, too many extensions, BEP 10 id=0 skip, exactly-`MAX_EXTENSIONS` boundary). No API break on `ExtensionRegistry::new`; the one caller in `session::peer` updated to `.map_err(|e| e.to_string())`.

### Added (M3 — Extension protocol + Magnet + PEX + LSD)

- BDD `.feature` files for BEP 9 (`ut_metadata`), 10 (extension protocol), 11 (PEX), 14 (LSD) under `crates/magpie-bt/tests/features/`. Each scenario maps to existing codec / assembler / actor behavior to lock the M3 contract.
- `docs/bep-coverage.md` rows for BEP 9 / 10 / 11 / 14 promoted `planned → done` and now link to their feature files.
- BEP 11 PEX-reachability fix: `PeerConfig::local_listen_port` (BEP 10 `p` field) is stamped per-attach from `Engine::listen()`, and inbound `ExtensionHandshake.listen_port` rewrites the per-peer `addr` to `(remote_ip, their_listen_port)`. Without this, PEX rounds advertise the inbound source ephemeral port and other peers can't dial back. Added `Engine::drain_pex_discovered(torrent_id)` accessor (proxied via new `SessionCommand::DrainPexDiscovered`) so consumers and tests can pump discoveries into `add_peer`. New `AddTorrentRequest::pex_interval` test knob lets the integration suite exercise PEX rounds in <1 s rather than 60 s.
- `tests/pex_discovery.rs` (M3 hard-gate criterion 4): three magpie engines, A seeds, B and C `add_peer` A only. PEX rounds on A advertise B↔C, both leechers complete via the resulting cross-connect, SHA-256 verified end-to-end.
- Magnet interop scenarios: `ci/interop/docker-compose.qbittorrent-magnet.yml` + `gate_qbittorrent_magnet.sh` and the equivalent Transmission pair feed a `magnet:?xt=urn:btih:...&tr=...` URI to the third-party leech, exercising magpie's BEP 9 `ut_metadata` server end-to-end. Seeder example gains `--advertise-metadata` (plumbs `info_dict_bytes` into `AddTorrentRequest` so the session advertises `metadata_size` in its extension handshake and serves piece requests). `generate_fixture` writes `fixture.magnet` alongside the existing `.torrent` artifacts. `run.sh` accepts the new `qbittorrent-magnet` / `transmission-magnet` scenario ids.

### Added (M2 — Stages 1–3, in progress)

Stage 1 (foundations):
- `magpie-bt-core::session::peer`: `read_handshake` + `write_handshake` primitives split out of `perform_handshake` for the inbound-accept path where `info_hash` is unknown until the peer's handshake is read.
- `magpie-bt-core::engine`: `Engine::listen(addr, ListenConfig)` inbound-TCP accept loop. Peer-ID collision: silent drop after reading peer's handshake (invariant #6). Pre-filter via `DefaultPeerFilter` on the remote address before handshake. Global peer cap (`Engine::with_global_peer_cap`, default 500) and per-torrent cap (`AddTorrentRequest::peer_cap`, default 50), enforced race-free via `fetch_add` + rollback. Lockstep `HashMap<InfoHash, TorrentId>` index for router dispatch. 6 passing integration tests.
- `magpie-bt-core::session::udp::demux`: `UdpDemux` actor with first-byte dispatch hook (M3 DHT + M4 uTP ready), tracker transaction-id routing, 60 s TTL sweep, 10 000 pending cap, 8 KiB recv buffer, 10 ms error backoff.
- `README.md`: reachability note — M2 inbound works on LAN or manually forwarded port; UPnP/NAT-PMP deferred to M5.

Stage 2 (upload path):
- `DiskOp::VerifyAndWrite::buffer` switched `Vec<u8>` → `Bytes` (ADR-0007 amendment). New `DiskOp::Read` with `oneshot` reply for the seed-side pull path.
- `session::read_cache`: piece-granular LRU, singleflight via `tokio::broadcast`, bypass path, `insert_verified` store-buffer hook. 5 tests.
- `session::peer_upload`: `PeerUploadQueue` — unread cap 128 + drop-newest, 2 s post-choke grace, fast-set abuse cap 3× blocks/piece, adaptive watermark. 13 tests.
- Seed-side messages + peer.rs wire handling + torrent actor seed logic using `read_cache.get_or_load` + `spawn-reader → BlockReady`.
- `AddTorrentRequest::initial_have` resume-from-disk API + `engine_seed.rs` integration test (external leecher receives correct bytes over TCP loopback).
- `session::choker` (ADR-0012): `Unchoker` trait, `LeechChoker` (tit-for-tat), `SeedChoker` (rasterbar `fastest_upload`), deterministic splitmix64 optimistic draw, 3× new-peer weight. 8 tests.
- `session::shaper` (ADR-0013): `TokenBucket` lock-free hot path, three-tier `Shaper`, 100 ms `Refiller`. Plan invariant #3 mechanically enforced. 10 tests.
- `Alert::{TorrentComplete, StatsUpdate}`; ADR-0019 completion transition guarded by single `completion_fired` flag; `NotInterested` broadcast (invariant #11 ordering).

Stage 3 (tracker upgrades + persistent stats + BDD):
- BEP 27 private flag: `TorrentParams::private` + `is_private()`, example wired to `meta.info.private`.
- `tracker::udp` (BEP 15): codec for CONNECT + ANNOUNCE, retry curve per spec, `CONNECTION_ID_TTL = 60 s` (invariant #10). 7 tests.
- `tracker::tiered` (BEP 12): `TieredTracker` with tier fall-through + promote-on-success (invariant #9) + deterministic Fisher-Yates shuffle. 4 tests.
- `session::stats`: `PerTorrentStats::retire_peer` enforces invariant #2 (`Release` on fetch_add paired with `Acquire` in `snapshot`). `StatsSink` + `FileStatsSink` (atomic write-tmp + rename bencode sidecar, info_hash dedup, 5 s bounded graceful flush). 6 tests.
- BDD `.feature` skeletons for BEP 12 / 15 / 27. `docs/bep-coverage.md` rows updated `planned → partial`.

Stage 21 (red-team polish pass):
- **Interop compose port publishing** (red-team): `docker-compose.qbittorrent.yml` + `docker-compose.transmission.yml` now publish webUI / RPC ports to `127.0.0.1:8080` / `127.0.0.1:9091`. Without this, host-side `gate_*.sh` scripts couldn't reach the container APIs. Localhost-only bind keeps the ports out of the docker host's external surface.
- **qBittorrent 4.6+ Referer handshake** (red-team): added `Referer: $WEB_HOST` headers to every `gate_qbittorrent.sh` request. Modern qBittorrent enforces this as an anti-CSRF check on the webUI API; omission returns 403 Forbidden.
- **Interop first-green-run notes**: `ci/interop/README.md` documents the two env-dependent bootstrap steps (capture qBittorrent temporary admin password from `docker logs`, freeze image digests on first pass). Matches `feedback_plan_red_team` silent-failure-fixture discipline — the gate can't pass without these one-time-per-image resolutions.
- **engine_e2e flake fix** (#25): `tests/engine_e2e.rs` had a 5 s completion deadline that was occasionally tight under `--all-features` parallel load (3 mock seeders + 1 engine leech + 20+ parallel test binaries fighting for runtime). Bumped to 15 s. 5 consecutive `cargo test --workspace --all-features` sweeps now pass clean.

Stage 20 (interop per-client gate scripts + workflow):
- `ci/interop/gate_qbittorrent.sh` — adds `fixture.torrent.with-announce` via the qBittorrent webUI API (`/api/v2/torrents/add`), polls `/api/v2/torrents/info` for completion state, extracts the downloaded file from the leech container, SHA-256s against the fixture. Retries webUI login for up to 60 s so the image has time to boot.
- `ci/interop/gate_transmission.sh` — same shape via Transmission's RPC API. Handles the anti-CSRF `X-Transmission-Session-Id` header flow (initial 409 response carries it). Uploads metainfo as base64, polls `torrent-get` until `status == 6` (seeding).
- `ci/interop/run.sh` now invokes `gate_${scenario}.sh` after stack boot and propagates its exit code; no more EX_CONFIG placeholder.
- `.github/workflows/interop.yml` — new workflow. PR + main + daily cron (scheduled 05:37 UTC so latest-tagged third-party images catch regressions within 24h) + manual dispatch. Matrix: `[qbittorrent, transmission]`. Uploads `/tmp/interop-{scenario}.log` on failure.
- Gate review updated: criterion 5 moved ⚠-scaffolded → ⚠-ready-to-run. Code-complete; flips to ✅ on first green CI run (blocks on a docker-capable runner + image-digest pinning on first pass).

Stage 19 (G1 pause atomicity + fast-ext shortcuts + interop scaffolding):
- **G1 pause atomicity tightening** (#20): `set_paused(true)` now broadcasts `Cancel` for every outstanding outbound `Request` and releases its `in_progress` claim. Without this, a pause would leave stale claims until peers naturally responded or timed out, and the remote would keep sending `Piece` responses for requests we've already decided to abandon. New unit test `g1_pause_cancels_outstanding_requests_and_releases_claims` asserts the Cancel broadcast + claim release.
- **HaveAll / HaveNone fast-ext shortcuts** (#21): `SessionCommand::RegisterPeer` carries a new `supports_fast: bool` (the AND of our `config.fast_ext` and the peer's handshake `supports_fast_ext()`). `register_peer_with` now emits `SendHaveAll` when `missing_count == 0`, `SendHaveNone` when `missing_count == total`, else `SendBitfield`. Engine spawn_peer_task populates the bool from the live handshake. 4 new tests covering each decision branch (complete + fast, empty + fast, partial + fast fallback, non-fast fallback).
- **Interop scaffolding** (#3): `ci/interop/` tree with `Dockerfile.magpie` (multi-stage workspace build), `docker-compose.qbittorrent.yml` + `docker-compose.transmission.yml` (static IPs on isolated bridge networks so the mock tracker can hand the leech a direct seeder IP), `run.sh` orchestrator, `README.md`. Three new magpie examples ship as the interop binaries:
  - `examples/mock_tracker.rs` — ~120-LOC HTTP BEP 3 / BEP 23 tracker. Returns a fixed compact peer list from `MOCK_TRACKER_PEERS` env. No swarm tracking, no scrape — drives only client-discovery.
  - `examples/generate_fixture.rs` — deterministic `.torrent` + data-file generator behind a new `test-support` Cargo feature on `magpie-bt` that re-exports metainfo's synthetic generator. Splices an `announce` URL into the outer bencode dict so third-party clients have somewhere to announce.
- `run.sh` exits 78 (EX_CONFIG) after boot so CI can't mistake the scaffolding-only state for a passing gate. The per-client add-torrent + SHA-256 completion check is filed as the next stage (#7).
- Compose files use `TODO: pin digest` on `linuxserver/qbittorrent:latest` and `linuxserver/transmission:latest` — first-green run freezes those.

Stage 18 (per-peer stats wiring + subprocess SIGKILL gate):
- `session::peer::PeerConn` gains a `peer_stats: Option<Arc<PeerStats>>` field + `with_peer_stats(...)` constructor. Hot-path increments at the same sites as the shaper's `try_consume` — `add_uploaded(bytes)` after successful `framed.send(Message::Piece(...))`; `add_downloaded(len)` on `Message::Piece` decode. Bumped **after** send so a wire error doesn't count as bytes-sent.
- `engine::TorrentEntry` owns `torrent_stats: Arc<PerTorrentStats>` + `live_peer_stats: Arc<StdMutex<HashMap<PeerSlot, Arc<PeerStats>>>>`. `spawn_peer_task` allocates a fresh `PeerStats`, inserts into the live-set, hands the Arc to PeerConn. On peer exit: **retire before remove** (plan invariant #2 — synchronises with the snapshot's Acquire loads).
- New public accessor `Engine::torrent_stats_snapshot(id) -> Option<StatsSnapshot>` — reads `PerTorrentStats::snapshot(live_peers)` which sums live counters with the retired-peer accumulator.
- `examples/seeder.rs`: new `--flush-secs N` flag (default 30; lower for tests). Periodic flush task now genuinely snapshots `torrent_stats_snapshot` → `sink.enqueue` → `flush_now`, so the FileStatsSink actually receives non-zero counters. Stdout stats line now prints real `uploaded=N downloaded=M` alongside disk counters. Also takes one final snapshot in the graceful-shutdown path so `flush_graceful` writes the freshest counters.
- New gate test `crates/magpie-bt-core/tests/stats_persist_subprocess.rs` (M2 gate criterion 6 — subprocess variant). `#[ignore]`'d. Spawns the seeder binary → drives an in-process leech to completion → polls the FileStatsSink sidecar until `uploaded > 0` (silent-failure-fixture guard) → **SIGKILL** the seeder (uncatchable; `flush_graceful` never runs) → respawns with the same `--stats-dir` → parses the restored "uploaded N down M" startup log line → asserts restored ≥ pre-kill snapshot AND `> 0`. End-to-end verified: uploaded 524288 bytes → SIGKILL → restored 524288.

Stage 17 (consumer-facing seeder example + dhat feature):
- `examples/seeder.rs` (new) — single-file v1 seeder binary. Opens an existing file as `FileStorage`, runs the engine in seed-only mode with `initial_have = all`, accepts inbound peers, optionally announces to the torrent's tracker, persists cumulative up/down via `FileStatsSink` (30s batch + 5s bounded graceful flush on SIGINT). Flags: `--torrent`, `--data`, `--listen`, `--stats-dir`, `--announce`, `--verify` (SHA-1 every piece at startup), `--allow-loopback` (dev-only peer-filter swap). Unix-only (FileStorage constraint); Windows builds emit a clear `Unix-only` message and exit 2.
- `dhat-heap` Cargo feature on `magpie-bt` (optional `dhat` dep via workspace pin). When enabled, seeder uses `dhat::Alloc` as global allocator and writes `dhat-heap.json` on graceful exit. Unblocks `ci/soak/dhat.sh`.
- Facade re-exports: `FileStatsSink`, `StatsSink`, `StatsSnapshot`, `ListenConfig` — consumer-surface additions that were previously buried under `magpie_bt_core::...`.
- `tests/seeder_example_smoke.rs` (new, `#[ignore]`'d) — builds the binary, spawns it, drives an in-process magpie leech to completion, asserts SHA-256 match across the subprocess boundary. Precursor to `#23` subprocess SIGKILL stats_persist (now unblocked).

Stage 16 (shaper hot-path wiring + throughput-floor gate):
- `session::shaper::bucket`: `TokenBucket` gains a `tokio::sync::Notify` field + `notify_refill()` / `wait_for_refill()` methods. Refiller calls `notify_refill()` after each `grant()` so peer tasks parked on denial wake exactly when tokens arrive — zero polling, no 10 ms busy-loop.
- `session::shaper`: `PeerBuckets.buckets` changes `DuplexBuckets → Arc<DuplexBuckets>` so peer tasks can cache a handle at startup and hit `try_consume` lock-free on the hot path. New `Shaper::peer_buckets(slot) -> Option<Arc<DuplexBuckets>>` accessor.
- `engine::Engine`: owns `Arc<Shaper>` + a dedicated `refiller_task: Mutex<Option<JoinHandle<()>>>`. Refiller task spawned in `new()`, aborted first in `join()` so the infinite refill loop doesn't deadlock teardown. `register_torrent_passthrough` on `add_torrent`; `drop_torrent` on `shutdown` / `remove`; `register_peer` + cache `Arc<DuplexBuckets>` handle on `spawn_peer_task`; `drop_peer` on peer task exit. New public `Engine::shaper() -> Arc<Shaper>` accessor so tests + consumers can pin per-peer rates.
- `session::peer::PeerConn`: new `with_shaper` constructor; hot-path `try_consume` calls wired at two points — (a) **upload** before `framed.send(Message::Piece(...))` with `wait_for_refill().await` on denial; (b) **download** after decoding `Message::Piece` as **accounting only** (bytes already received; real download shaping would gate the next `framed.next()` and is deferred to M5 cap-enablement). Plan invariant comment on both sites: **peer tier only on the hot path**; session + torrent tiers touched exclusively by the refiller (ADR-0013).
- New test `session::shaper::bucket::tests::wait_for_refill_wakes_on_notify` — parks on empty bucket, asserts waiter wakes + succeeds after `grant` + `notify_refill`.
- New gate test `crates/magpie-bt-core/tests/throughput_floor.rs` (M2 gate criterion 2b) — pins the seed's peer up-bucket at 1 MiB/s, runs magpie↔magpie over loopback, asserts observed ≥ 0.80 × pinned rate and ≤ 2.0 × pinned rate (ceiling catches shaper bypass). 5/5 consecutive runs green. controlled_swarm re-run 10/10 clean.
- `docs/milestones/002-seeder-multi-torrent.md` gate criterion 2b → ✅.

Stage 5–10 (close-out verification + API completion):
- **API completion (consumer-surface audit follow-ups)**: `Engine::pause` / `resume` (G1, idempotent, `TorrentNotFoundError`); `Engine::remove(id, delete_files)` (G2, path-safe via consumer-resolved paths, calls new `Storage::delete` default-Ok trait method); `Engine::torrents()` + `torrent_state(id) -> Option<TorrentStateView>` (G3, `#[non_exhaustive]` snapshot). 14 new integration tests across `tests/engine_g{1,2,3}.rs` + 4 actor-level unit tests for pause broadcast scope. Facade re-exports `TorrentStateView`, `TorrentNotFoundError`. `Storage` trait gains `fn delete(&self) -> Result<(), StorageError>` (default `Ok(())`); `FileStorage` overrides to unlink its construction path; `MemoryStorage` no-ops via default.
- **Major bug fix — seed-side initial advert**: M2-as-shipped seed peer task never sent post-handshake `Bitfield`/`HaveAll`/`HaveNone`, so leech actors never knew the seed had pieces and any magpie↔magpie integration sat idle. Fixed by sending `SessionToPeer::SendBitfield(bytes)` from `register_peer_with` (race-free vs `Connected` event ordering). Three new `SessionToPeer` variants (`SendBitfield`, `SendHaveAll`, `SendHaveNone`) wired through `peer.rs` message loop. New `pack_bitfield()` helper. Six existing actor-level tests updated to drain the initial advert via a `drain_initial_advert` helper.
- **Tighter G1 pause invariant**: `handle_peer_interest` now suppresses auto-unchoke while paused so a new-peer-becomes-interested event during pause doesn't race-unchoke.
- **Synthetic torrent generator (test-support)**: new `magpie-bt-metainfo::test_support` module behind a `test-support` Cargo feature (semver-exempt; banner in module + Cargo.toml + README "Stability" section). Deterministic single-file v1 torrent generator using splitmix64 PRNG content + SHA-1 piece hashes. 8 unit tests including parse-back, determinism, BEP 52 power-of-two enforcement.
- **`session::torrent` ADR-0019 ordering tests**: 4 tests — alert-then-broadcast, broadcast scope (interested peers only), guard idempotency under re-armed `we_are_interested`, resume-from-complete skip path. Tests pin the invariants observable today; follow-up #19 extends them when steps 2/3/5 (choker swap + tracker `event=completed`) wire in.
- **UDP tracker fuzz target**: new `crates/magpie-bt-core/fuzz/fuzz_targets/udp_tracker.rs` over `decode_connect` + `decode_announce`; 5-file seed corpus.
- **Nightly fuzz workflow bug fix**: `nightly.yml` preflight checked for a repo-root `fuzz/` dir that never existed, so the fuzz matrix had been silently skipped since it landed. Rewritten with `working-directory: crates/<crate>/fuzz` matrix entries; six fuzz targets (`bencode`, `metainfo`, `handshake_decode`, `wire_decode`, `alert_ring`, `udp_tracker`) now actually run nightly.
- **BDD step definitions for BEP 12 / 15 / 27**: `tests/steps/bdd_extra.rs` wires the previously-skipped 13 scenarios. 24/25 cucumber scenarios now green; 1 `@deferred`-tagged scenario (UDP-client `connection_id` refresh) needs the high-level UDP client wrapper that's a separate follow-up. `docs/bep-coverage.md` rows updated: BEP 12 → done, BEP 15 → partial (with deferred-scenario note), BEP 27 → done.
- **`controlled_swarm` magpie-only hard gate**: `tests/controlled_swarm.rs` — magpie-seed↔magpie-leech over loopback, synthetic 1 MiB content from `test-support`, SHA-256 match. 10/10 consecutive runs green; covers M2 gate criterion 2.
- **Stats persistence**: new `FileStatsSink::load_sidecar` parses bencode sidecars back into `StatsSnapshot`. `tests/stats_persist.rs` (4 tests) covers round-trip non-zero counters, cold start, truncation rejection, last-flush-wins. Subprocess SIGKILL variant filed as #23 (no magpie binary today).
- **Prometheus exporter**: new `magpie_bt_core::metrics_exporter::render_disk_metrics` behind a `prometheus` Cargo feature. **Pure stdlib** — no `prometheus` crate dep, so enabling adds zero transitive build cost. Renders `magpie_disk_*` counters in Prometheus text-exposition format with `torrent` label. 3 tests.
- **Soak harness scaffolding**: `ci/soak/multi-torrent.sh` + `tests/soak_multi_torrent.rs` (`#[cfg(unix)]`, `#[ignore]`). N=8 magpie pairs running concurrent seed↔leech cycles for `SOAK_DURATION_SECS`, asserting per-cycle SHA-256 match. Smoke-verified locally (5 cycles × 4 pairs in 10s green). `ci/soak/dhat.sh` exits 78 (`EX_CONFIG`) until a dhat-instrumented soak binary lands — silent-pass safety net.
- **Weekly soak workflow**: `.github/workflows/weekly-soak.yml` with Sunday cron + `workflow_dispatch` overrides. Two jobs: `multi-torrent` (real run) and `dhat` (`continue-on-error: true` until the dhat binary lands).
- **Documentation**: `docs/api-audit.md` (consumer-surface audit), `docs/RSS-budget.md` (methodology + TBD-marked table for the empirical number), `docs/m2-gate-review.md` (final gate scorecard).

Stage 4 (close-out verification — in progress):
- `docs/api-audit.md`: consumer-surface audit walking magpie-bt public API vs realistic client call sites (lightorrent, rasterbar, anacrolix). Surface is client-agnostic; three additive completeness gaps filed as tasks: `Engine::pause`/`resume` (G1), `Engine::remove(id, delete_files)` with path-traversal safety (G2), `Engine::torrents()` + `torrent_state()` snapshot API (G3).
- `session::torrent`: ADR-0019 completion-transition ordering unit tests (4 tests). Pin the invariants observable today (step 1 alert, step 4 `NotInterested` broadcast scope, guard idempotency across re-armed peers, resume-from-complete skip path). Follow-up task tracks extension when steps 2/3/5 wire in.
- `magpie-bt-core/fuzz/fuzz_targets/udp_tracker.rs` (new): BEP 15 response-decoder fuzz target over `decode_connect` + `decode_announce`; 5-file seed corpus covers valid CONNECT + ANNOUNCE (2 peers) and the malformed branches (short payload, action=ERROR).
- `.github/workflows/nightly.yml`: **bug fix** — previous preflight checked for a repo-root `fuzz/` dir that never existed, so the fuzz job was silently skipped. Rewritten to use `working-directory: crates/<crate>/fuzz` with an explicit crate×target matrix. All six currently-existing fuzz targets (`bencode`, `metainfo`, `handshake_decode`, `wire_decode`, `alert_ring`, `udp_tracker`) now run nightly at the DISCIPLINES ≥10 min cadence.
- `docs/MILESTONES.md`, `docs/ROADMAP.md`, `docs/milestones/002-seeder-multi-torrent.md`, `docs/PROJECT.md`, `CLAUDE.md`, `docs/DISCIPLINES.md`: M2 reframed as hermetic ("Seeder + multi-torrent, consumer-integration ready"). Consumer integration (lightorrent) is explicitly out of M2 scope — it's the consumer's repo/timeline. `ROADMAP.md` M5 gate reworded from "remove librqbit from lightorrent" to "client-replacement readiness demonstrated via a production-grade consumer cutover". `DISCIPLINES.md` + `PROJECT.md` reframe API-shape discipline as client-agnostic with lightorrent as a completeness check, not the shape driver.

Gates (running): 22 test suites + 125 unit tests (121 pre-stage-4 + 4 ADR-0019 ordering) + `cargo clippy --workspace --all-targets -- -D warnings` clean.

Deferred to Stage 5 / gate review (lightorrent trait extraction is **explicitly out of scope** — consumer repo concern): docker interop scenarios (qBittorrent/Transmission) with local tracker + synthetic fixtures; rate-shaped throughput harness (`throughput_floor` test, magpie↔magpie loopback); 24 h soak workflow (`weekly-soak.yml`) + Prometheus exporter + `docs/RSS-budget.md`; stats-persist subprocess test; controlled-swarm magpie-only gate test; G1–G3 API completeness additions (`pause`/`resume`/`remove`/`torrents`); extending ADR-0019 ordering tests when choker/tracker wire in; UDP tracker client that wraps codec + demux into connect→announce with `connection_id` cache; 1 Hz `StatsUpdate` emitter task; `PeerUploadQueue` + shaper wired into the hot path; choker rotation timer wired to the torrent actor; store-buffer short-circuit auto-populating the cache on `DiskCompletion`; tracker `event=completed` hook.

### Added (M1 — live-network leech proof)
- `magpie-bt/examples/leech.rs`: real-network leecher example. Parses a single-file v1 .torrent, opens a `FileStorage` sized to `total_length`, attaches the announce URL as an `HttpTracker`, runs to completion with progress reporting + ctrl-c shutdown. Built via `cargo run --example leech --release -- --torrent FILE.torrent --out FILE.iso`.
- **Live verification**: ran the example against `debian-13.4.0-amd64-netinst.iso.torrent` (754 MiB / 3016 pieces, public tracker `http://bttracker.debian.org:6969/announce`). Result: **bit-perfect download in 4 min 27 s at 2.85 MiB/s sustained from 13 simultaneous peers, zero hash failures, final SHA-256 matches Debian's published checksum**. M1 milestone gate criterion #2 ("fetch a real public-tracker torrent end-to-end over TCP with all v1 hashes verified") is empirically met.
- `magpie-bt-core::engine`: per-peer TCP connect now wrapped in `tokio::time::timeout(DEFAULT_PEER_CONNECT_TIMEOUT = 5s)`. Without this, NAT'd / firewalled tracker peers pinned individual `add_peer` tasks for the OS-default ~75 s before timing out — observably the difference between 1 and 13 connected peers in the live test.
- `magpie-bt-core::engine::attach_tracker`: announce-loop now fans out `add_peer` calls in parallel via `tokio::spawn` instead of awaiting them serially. A typical 50-peer announce response no longer spends 250 s walking through connect timeouts before the first peer attaches.
- New constant: `magpie_bt_core::engine::DEFAULT_PEER_CONNECT_TIMEOUT`.

### Added (M1 phase 6 — instrumentation + hygiene)
- **Tracing spans** on every actor: `tracing::info_span!("torrent", piece_count, total_length)` around `TorrentSession::run`, `info_span!("peer", slot, supports_fast)` around `PeerConn::run`, `info_span!("disk_writer")` around `DiskWriter::run`. `tracing::debug!`/`info!`/`warn!`/`error!` events at lifecycle transitions (peer connect/disconnect with reason, piece verified/hash-mismatch, storage IO failure, engine add_torrent / add_peer / filter rejection).
- `magpie-bt-core::engine` (E1): per-torrent registry switched from `tokio::sync::Mutex<HashMap>` to `tokio::sync::RwLock<HashMap>`. Concurrent `add_peer` / `disk_metrics` / `snapshot` reads no longer serialise behind `add_torrent` / `shutdown` writes.
- `magpie-bt-core::engine` (E12): `TorrentId(u64)` inner field is now private. Callers can no longer mint forged ids.
- `magpie-bt-core::engine` (E13): `Engine` doc-comment clearly states the lifecycle contract — `shutdown(id)` for every torrent then `join()` before drop, otherwise spawned tasks become tokio-runtime orphans.
- `magpie-bt-core::peer_filter` (E10): `DefaultPeerFilter` doc-comment now calls out the loopback-rejection pitfall and points test consumers at `permissive_for_tests()`, with a runnable example.

### Deferred to M2 (documented)
- E2 (inbound-listener API), E5 (`join` race), E6 (return slot id from `add_peer`), E26 (surface task panics): wait for the seeder side to land in M2, where the API trade-offs are clearer.
- D2 (concurrent disk writer), D3 (storage timeouts), D5 (drain semantics on Shutdown), D6 (metrics rename / extra counters), D9 (`DiskWriterHandle` factory): ergonomics + perf, defer.
- Optional Prometheus `metrics` feature (Phase 6 plan item): `DiskMetrics` already exposes the data via atomics; consumers wrap in their preferred metrics library. Magpie-shipped exporter deferred to M2 to align with multi-torrent semantics.
- `cargo llvm-cov` coverage measurement (Phase 6 plan item): defer until CI infrastructure is in place; coverage bars in DISCIPLINES.md remain aspirational targets.
- Live Ubuntu ISO fetch (`live_ubuntu.rs`) + `dhat_leak.rs`: feature-gated test stubs, deferred until nightly CI workflow exists.

### Hardened (M1 phase 5 red-team — must-fix trio)
- `magpie-bt-core::session::torrent` (E14, **panic surface**): new `TorrentParams::validate()` plus `TorrentParamsError` enum. Rejects zero `piece_count` / `piece_length` / `total_length`, `piece_hashes.len() != 20 * piece_count`, and `total_length > piece_count * piece_length`. `Engine::add_torrent` now returns `Result<TorrentId, AddTorrentError>` and validates up-front, closing a hard panic surface inside `finalise_piece` (out-of-range piece-hash slice).
- `magpie-bt-core::engine` (E16, **SSRF bypass**): `Engine::add_peer_stream` signature gains a `peer_addr: SocketAddr` parameter and now applies the configured `PeerFilter` before attaching. The previous filter-bypass test convenience is gone — callers using duplex pipes or accepting inbound connections must supply a representative address (or use `DefaultPeerFilter::permissive_for_tests`).
- `magpie-bt-core::engine` (E17, **tracker integration gap**): new `Engine::attach_tracker(self: &Arc<Self>, torrent_id, tracker, AttachTrackerConfig)` spawns a periodic announce loop that runs `Started` then `Periodic` events, applies the torrent's `PeerFilter` to every returned address (via `Engine::add_peer`), backs off on tracker errors via `AttachTrackerConfig::error_backoff` (30 s default), and respects `AnnounceResponse::clamped_interval` (30 s floor). Tracker failures publish `Alert::Error { code: TrackerFailed }`.
- `magpie-bt-core::tracker`: `Tracker` trait reshaped for dyn-compatibility — `announce` returns the new public `AnnounceFuture<'a> = Pin<Box<dyn Future<...> + Send + 'a>>` so `Engine` can hold `Arc<dyn Tracker>`. `HttpTracker::announce` now returns `Box::pin(async move { … })`.
- `magpie-bt-core/tests/engine_e2e.rs`: 2 new integration tests — `engine_attach_tracker_drives_announce_loop_and_filters_peers` (mock tracker drives the leech end-to-end via the announce loop) and `engine_rejects_invalid_torrent_params` (E14 guard surfaces as `AddTorrentError::InvalidParams`).
- 5 new unit tests on `TorrentParams::validate` covering each rejection class plus the happy path.
- Facade re-exports: `AddPeerError`, `AddTorrentError`, `AttachTrackerConfig`.

### Added (M1 phase 5 — end-to-end integration)
- `magpie-bt-core::engine`: new public surface — `Engine`, `EngineHandle`-style API surface (`add_torrent`, `add_peer`, `add_peer_stream`, `disk_metrics`, `shutdown`, `join`), `AddTorrentRequest`, `TorrentId`, typed `AddPeerError`. The Engine wires `DiskWriter` + `TorrentSession` together per torrent and applies the configured `PeerFilter` on every `add_peer` call before TCP-connecting.
- `magpie-bt-core::session::SessionCommand`: out-of-band command channel for the actor — `RegisterPeer { slot, tx, max_in_flight }` + `Shutdown`. Returned alongside the session from `TorrentSession::new`. `SESSION_COMMAND_CAPACITY = 16`.
- `magpie-bt-core/tests/engine_e2e.rs`: end-to-end integration test against **real TCP loopback** sockets — 3 in-process synthetic seeders, full BEP 3 handshake, leecher fetches a 128 KiB synthetic torrent, byte-equality + disk-metrics assertion + filter-rejection test. Completes in ~30 ms.
- `magpie-bt::tracker::parse_response`: public response decoder for use by custom transports / BDD steps.
- `crates/magpie-bt/tests/features/`: real BDD scenarios for **BEP 3** (handshake reserved bits, Have/Bitfield/Request round-trip + canonical layout), **BEP 6** (Fast bit, HaveAll/HaveNone/AllowedFast/RejectRequest), and **BEP 23** (compact v4, BEP 7 v6, failure-reason). 12 scenarios, 38 steps, all passing.
- `crates/magpie-bt/tests/steps/{wire,tracker}.rs`: corresponding step definitions.
- `magpie-bt` facade re-exports the M1 surface — `engine`, `peer_filter`, `session`, `tracker`, `wire` modules and root-level convenience exports `Engine`, `AddTorrentRequest`, `TorrentId`, `DefaultPeerFilter`, `PeerFilter`, `TorrentParams`, `TorrentState`, `HttpTracker`, `Tracker`.
- `docs/bep-coverage.md`: BEP 3 / 6 / 23 flipped from `planned` → `done`.

### Hardened (M1 phase 4 red-team pass + Phase-5 prep)
- `magpie-bt-core::session::disk` (D1, **deadlock fix**): `DiskCompletion` channel switched from bounded `mpsc::channel(64)` to `mpsc::unbounded_channel`. Bounding both legs of the actor↔writer pair could deadlock: actor blocked on `disk_tx.send` (forward queue full) cannot drain `completion_rx`; writer blocked on `completion_tx.send` (return queue full) cannot drain `disk_rx`. Outstanding completions are naturally capped by `DEFAULT_DISK_QUEUE_CAPACITY`, so the return leg cannot accumulate beyond that bound. `DEFAULT_DISK_COMPLETION_CAPACITY` constant removed; `DiskOp::VerifyAndWrite::completion_tx` is now `mpsc::UnboundedSender<DiskCompletion>`. ADR-0007 updated.
- `magpie-bt-core::session::torrent` (H1): `decode_bitfield_strict` spare-bit formula simplified — redundant inner `% 8` removed (`piece_count % 8` is already in `[0,7]`).
- `magpie-bt-core::peer_filter` (T3): new module exposing the `PeerFilter` trait + `DefaultPeerFilter` implementation. Default rejects loopback, IPv4 link-local, IPv6 link-local (`fe80::/10`), unspecified, multicast, IPv4 broadcast, port-0 peers; permits RFC 1918 / unique-local v6 by default for LAN-seeding workflows. `DefaultPeerFilter::strict()` denies private addresses too; `DefaultPeerFilter::permissive_for_tests()` allows loopback. The future `Engine` (Phase 5) wires this between `tracker::AnnounceResponse::peers` and `TorrentSession::register_peer`. 9 unit tests cover the rejection classes.

### Added (M1 phase 4 — disk backpressure)
- `magpie-bt-core::session::disk`: new `DiskWriter` task that decouples SHA-1 verification + storage writes from the per-torrent actor. Public types: `DiskWriter`, `DiskOp`, `DiskCompletion`, `DiskError { HashMismatch | Io }`, `DiskMetrics { pieces_written, bytes_written, piece_verify_fail, io_failures }`. Constants: `DEFAULT_DISK_QUEUE_CAPACITY = 64`, `DEFAULT_DISK_COMPLETION_CAPACITY = 64`.
- `magpie-bt-core::session::torrent`: `TorrentSession` no longer holds an `Arc<dyn Storage>` directly. New constructor signature: `new(params, alerts, peer_rx, disk_tx)`. The actor's `select!` drains both `PeerToSession` and `DiskCompletion` channels; `mark_have` + `PieceCompleted` fire only when disk acknowledges. Phase-3 inline `spawn_blocking` band-aid removed.
- End-to-end backpressure chain established: full disk queue → actor `send().await` blocks → peer→session channel fills (S1 cap) → peer task stops reading wire → TCP window closes upstream. No explicit semaphore needed for M1.
- "Awaiting verification" gap closed: pieces remain in `in_progress` (buffer moved out) until the matching `DiskCompletion` arrives, preventing the scheduler from re-requesting blocks during the verify window.
- ADR-0007 (disk-write backpressure) — accepted.
- 2 new unit tests in `disk.rs` (verify-and-write happy path, hash-mismatch surfacing). Integration test now asserts `DiskMetrics` reflects every committed piece.

### Hardened (M1 phase 3 red-team pass)
- `magpie-bt-core::session::peer` (S1): `PeerToSession` channel switched from `mpsc::unbounded_channel` to `mpsc::channel(PEER_TO_SESSION_CAPACITY = 64)`. The peer task's `send().await` now naturally backpressures the wire reader and TCP, satisfying ADR-0002's "bounded MPSC for high-volume per-block streams" rule. `SessionToPeer` stays unbounded (rate naturally bounded by per-peer in-flight cap).
- `magpie-bt-core::session::torrent` (S2/S3 band-aid): `finalise_piece` moved off the actor task — SHA-1 verification + storage write now run on `tokio::task::spawn_blocking`, so a slow disk no longer freezes peer-event processing for the entire torrent. Will be subsumed by Phase 4's `DiskWriter` task.
- `magpie-bt-core::session::torrent` (S4 + S5): `Bitfield` validation enforces BEP 3 length (`ceil(piece_count / 8)`) and the spare-bit-zero invariant. Misbehaving peers are kicked with `Alert::Error { code: PeerProtocol }`.
- `magpie-bt-core::session::torrent` (S6): out-of-range piece in `Have` / `BlockReceived` is now a protocol violation, peer is disconnected.
- `magpie-bt-core::session::torrent` (S7 + S8): `Piece` payloads are accepted only when `claimed[idx] == Some(slot)` (i.e. this peer was actually asked). Unsolicited blocks → kick. `peer.in_flight` is decremented only after the block is accepted, closing the counter-drift surface.
- `magpie-bt-core::session::torrent`: new `kick_peer(slot, code)` helper centralises the "drop peer + emit Error alert" pattern. The peer task receives `Shutdown`; the regular `Disconnected` flow performs cleanup.
- `magpie-bt-core::session::peer` (S16): `tx_to_session.send` failure (session has dropped) terminates the peer task with `DisconnectReason::Shutdown` instead of silently leaking it.
- `magpie-bt-core::session::peer` (S17): `perform_handshake` now wraps the exchange in `tokio::time::timeout(handshake_timeout)`. New `PeerConfig::handshake_timeout` field (default 10 s, exposed as `DEFAULT_HANDSHAKE_TIMEOUT`); new `HandshakeError::Timeout` variant.
- `magpie-bt-core::session::peer` (S24): single source of truth for `DEFAULT_PER_PEER_IN_FLIGHT` lives on `peer.rs`; the torrent actor imports it. New `TorrentSession::register_peer_with(slot, tx, max_in_flight)` lets callers override; the existing `register_peer(slot, tx)` continues to use the default.
- `magpie-bt-core::tracker` (T2 floor): new `MIN_REANNOUNCE_INTERVAL = 30 s` and `AnnounceResponse::clamped_interval()` helper. Sessions clamp tracker-returned intervals up before scheduling, defending against a tracker that returns the spec-legal but abusive `interval: 1`.
- New unit tests for `decode_bitfield_strict`: well-formed accept, wrong length reject, non-zero spare-bits reject, byte-aligned-count happy path.

### Added (M1 phase 3 — session scaffold)
- `magpie-bt-core::session`: new module orchestrating per-torrent leeching.
  - `TorrentSession` actor: owns `Picker`, in-progress piece buffers, peer registry; verifies SHA-1, commits to `Storage`, broadcasts `Have` to peers, emits `Alert::PieceCompleted` / `PeerConnected` / `PeerDisconnected` / `Error{HashMismatch|StorageIo}`. Reaches `TorrentState::Completed` when all pieces verify.
  - `PeerConn<S>` task: generic over any `AsyncRead + AsyncWrite + Unpin + Send + 'static` transport (works with `TcpStream` and `tokio::io::duplex` alike). Drives `Framed<S, WireCodec>`, handles the BEP 3 + BEP 6 message set, enforces a per-peer in-flight cap, propagates `Choke`-clears-in-flight per BEP 3 (gated by negotiated Fast bit).
  - `perform_handshake(stream, config, role)` helper for both initiator and responder roles, including info-hash mismatch detection.
  - Public types: `TorrentParams`, `TorrentState`, `PeerSlot`, `PeerConfig`, `HandshakeRole`, `HandshakeError`, `PeerToSession`, `SessionToPeer`, `DisconnectReason`.
  - Internal: greedy block-claim scheduler with per-block ownership tracking, ready to gain endgame mode in Phase 5.
- `magpie-bt-core/tests/session_duplex.rs`: end-to-end leecher integration test — handshake → bitfield → request → piece → SHA-1 verify → storage → completion across an in-process duplex pipe, no real sockets.
- ADR-0009: peer connection state machine + Fast extension — accepted.
- ADR-0010: request pipelining (M1 baseline = fixed 4 in-flight; BDP ramp + endgame staged for Phase 4/5) — accepted.
- Workspace dependency: `bytes` re-added to `magpie-bt-core` (used by session block payloads); `futures-util` `sink` feature enabled for `Framed::send`.

### Hardened (M1 phase 1+2 red-team pass)
- `magpie-bt-wire` (W1): `DEFAULT_MAX_PAYLOAD` lowered from 1 MiB to 256 KiB. The decoder no longer pre-reserves the announced payload size from the 4-byte length prefix alone, removing a per-connection buffer-amplification DoS where a peer sending only the prefix could grow each connection's buffer to the codec ceiling. New `WireCodec::set_max_payload` lets sessions widen the ceiling once the metainfo bitfield length is known.
- `magpie-bt-wire` (W2): `Piece` decode now caps payload at `BLOCK_SIZE + 8 = 16 400` bytes regardless of the codec ceiling, enforcing the v2 16 KiB block invariant on the wire.
- `magpie-bt-wire` (W3, W4): documented caller responsibilities on `Message`/`Bitfield` — bitfield length + spare-bit zero invariant must be enforced by the session, and BEP 6 messages must be dropped if the handshake did not negotiate Fast extension support.
- `magpie-bt-core::tracker` (T1): `HttpTracker` now streams the response body through `read_bounded_body` with a 4 MiB cap, rejecting both `Content-Length`-advertised oversize and chunk-streamed overflow.
- `magpie-bt-core::tracker` (T2): `parse_announce_response` rejects non-positive `interval` and `min interval`, closing a tight-loop reannounce amplification surface.
- `magpie-bt-core::tracker` (T4): `Client` redirect policy capped at 3 hops with explicit HTTPS→HTTP downgrade rejection.
- `magpie-bt-core::tracker` (T5): split timeouts — 5 s `connect_timeout` plus 30 s overall request timeout.
- `magpie-bt-core::tracker`: client now sends `User-Agent: magpie/<version>` (cosmetic; some trackers reject UA-less clients).
- Workspace dependency: `futures-util = "0.3"` (default-features off; needed for `reqwest::Response::bytes_stream`). `reqwest` `stream` feature added.

### Added (M1 phase 2 — HTTP tracker)
- `magpie-bt-core::tracker`: new module exposing the `Tracker` trait, `AnnounceRequest`, `AnnounceResponse`, `AnnounceEvent`, and `TrackerError`.
- `magpie-bt-core::tracker::HttpTracker`: `reqwest` + `rustls-tls` transport (per ADR-0011) with binary-safe BEP 3 percent-encoding (`build_announce_url`), bencoded response decode (compact BEP 23 v4 + BEP 7 v6 + dict-form peers), and `failure reason` propagation.
- ADR-0011: tracker HTTP transport — `reqwest` + `rustls-tls` accepted.
- Workspace dependency: `reqwest = { default-features = false, features = ["rustls-tls", "http2"] }`.

### Added (M1 phase 1 — peer wire codec)
- `magpie-bt-wire`: BEP 3 + BEP 6 framing. New public surface: `Handshake` (with BEP 6 `with_fast_ext` and BEP 10 `with_extension_protocol` reserved-bit helpers, `HANDSHAKE_LEN`, `PSTR`, `PSTRLEN`); `Message` enum covering keepalive, choke/unchoke, interested/not-interested, have, bitfield, request, piece, cancel, BEP 6 fast-extension messages (`HaveAll`, `HaveNone`, `SuggestPiece`, `RejectRequest`, `AllowedFast`), and an opaque BEP 10 `Extended` envelope; `BlockRequest`, `Block`, and the v2 `BLOCK_SIZE` invariant constant; `WireCodec` implementing `tokio_util::codec::{Decoder, Encoder}` with a configurable per-message ceiling (`DEFAULT_MAX_PAYLOAD = 1 MiB + 16`); typed `WireError`.
- `magpie-bt-wire`: `tests/proptest.rs` — encode/decode round-trip and never-panic-on-arbitrary-bytes properties.
- `magpie-bt-wire`: `fuzz/` workspace with `wire_decode` and `handshake_decode` cargo-fuzz targets (gate criterion #4).
- Workspace dependencies: `bytes = "1"`, `tokio-util = "0.7"` (codec feature only).

### Added (M0 phase D — integration + gate)
- `magpie-bt` facade: re-exports the M0 public API surface (`bencode`, `metainfo`, `alerts`, `peer_id`, `picker`, `storage` modules) plus convenience root re-exports (`parse`, `InfoHash`, `MetaInfo`, `AlertQueue`, `PeerIdBuilder`, `Picker`, `MemoryStorage`, `FileStorage` (Unix), `Storage`).
- Runnable rustdoc example on the `magpie-bt` crate root.
- ADR-0001 reconfirmed at M0 close: subcrate-per-protocol stance stands for M3 DHT and M4 uTP.
- `docs/milestones/001-leecher-tcp-v1.md`: M1 scope stub (planned).
- `docs/MILESTONES.md`: M0 flipped `in-progress → done`; M1 flipped `not-started → planned`.
- Scope note: M0 ships Unix-only; Windows storage backend is M1+.

### Added
- Initial documentation scaffold: `README.md`, `CLAUDE.md`, `docs/PROJECT.md`, `docs/ROADMAP.md`, `docs/MILESTONES.md`, `docs/milestones/000-foundations.md`.
- Quality disciplines baseline: `docs/DISCIPLINES.md`, `docs/adr/` scaffold, GitHub CI + nightly workflows, rustfmt/clippy/deny configs, security + contributing policies.
- Dual `Apache-2.0 OR MIT` licensing.

### Changed
- `rustfmt.toml`: edition `2021` → `2024`.
- `clippy.toml`: MSRV placeholder `1.75` → `1.94` (matches latest stable on dev machine, rustc 1.94.1).
- M0 status moved `planned` → `in-progress`.
- ADRs 0001/0002/0003 accepted. ADR 0002 reworked around a custom rasterbar-style alert ring (see [docs/adr/0002-event-bus-alert-ring.md](docs/adr/0002-event-bus-alert-ring.md)).

### Added (M0 phase C — core primitives, post-review hardening)
- `magpie-bt-core::picker`: `observe_peer_bitfield` now uses `saturating_add`, restoring symmetry with `forget_peer_bitfield`. A malicious-peer observation storm can no longer panic (debug) or wrap (release). Regression covered by `tests/picker_proptest.rs`.
- `magpie-bt-core::storage::file`: scoped to Unix only for M0 (`#[cfg(unix)]`). The Windows `FileExt::seek_write` semantics differ from `write_at` and would race under concurrent peer writes; a proper backend (overlapped I/O / `io_ring`) lands post-M0.
- Tests: `file_storage_concurrent_non_overlapping` — 4-thread concurrent-write correctness on Unix. `picker_proptest.rs` — observe/forget symmetry, missing-count consistency, pick-returns-missing invariant, saturation regression.
- Doc: `AlertQueue::wait` — clarified that `tokio::sync::Notify` does not produce spurious wakes; the loop handles stale permits.

### Added (M0 phase C — core primitives)
- ADR-0008 (`docs/adr/0008-vectorised-file-io.md`, proposed): placeholder for future `libc::pwritev`/`preadv` integration behind the `magpie-bt-core` unsafe allowlist. `magpie-bt-core` remains `unsafe`-free as of Phase C.
- `magpie-bt-core::peer_id`: `PeerIdBuilder` for 20-byte Azureus-style peer-IDs (`-CCVVVV-<12-byte-suffix>`). Convenience constructor `PeerIdBuilder::magpie` uses client code `Mg`. OS entropy via `getrandom`. Deterministic `build_with_suffix` for tests.
- `magpie-bt-core::alerts`: custom single-primary-reader alert ring (ADR-0002). Bounded `VecDeque`-backed buffer with drop-oldest overflow policy; `Alert::Dropped { count }` sentinel prepended on drain. Category masks (`PIECE`, `PEER`, `TRACKER`, `ERROR`, `STATS`, `ALL`, `NONE`) filter at push time. Async `wait()` via `tokio::sync::Notify`. `generation` counter increments on drain.
- `magpie-bt-core::storage`: `Storage` trait + `MemoryStorage` (Vec-backed) + `FileStorage` (stdlib `FileExt::{read_at,write_at}`, no unsafe). Typed `StorageError { kind: StorageErrorKind::{OutOfBounds, Io} }`. Default `writev`/`readv` compose scalar ops — vectorised `preadv`/`pwritev` deferred to ADR-0008.
- `magpie-bt-core::picker`: rarest-first piece picker with endgame threshold (default 5%). Tracks per-piece `availability` and `have`; `pick` / `pick_n` return rarest-first in normal mode, lowest-index missing in endgame. `observe_peer_bitfield` / `forget_peer_bitfield` maintain counters.
- Integration tests for gate criteria: `storage_roundtrip.rs` (#2), `picker_synthetic.rs` (#3), `peer_id_layout.rs` (#4).
- Benches: `alert_ring.rs` (push / overflow / drain / masked_push); `picker.rs` (pick / pick_n / progress); baselines in `benches/BASELINE.md`.
- Fuzz: `alert_ring` target wired to push/drain/set-mask invariants with seed corpus.
- Workspace: added `getrandom` to workspace deps; `tempfile` as dev-dep of `-core`.

### Added (M0 phase B — metainfo)
- `magpie-bt-metainfo`: `parse(&[u8]) -> Result<MetaInfo, ParseError>` for BEP 3 (v1), BEP 52 (v2), and hybrid torrents. Zero-copy `MetaInfo<'a>` with borrowed byte strings.
- `magpie-bt-metainfo`: `InfoHash::{V1, V2, Hybrid}` with SHA-1 and SHA-256 computed over the **raw info-dict span** (via `magpie_bt_bencode::dict_value_span`) rather than a re-encode — preserves byte-identity with the source `.torrent`.
- `magpie-bt-metainfo`: `MetaInfo::info_bytes` exposes the hashed span for BEP 9 relay / verification.
- `magpie-bt-metainfo`: typed `ParseError { kind: ParseErrorKind }` covering missing/wrong-type fields, non-power-of-two piece length, bad pieces blob length, unsupported meta version, conflicting v1 layout, malformed v2 file tree, invalid path components.
- `magpie-bt-metainfo`: v2 file tree parsed into nested `FileTreeNode::{File, Dir}` with 32-byte merkle `pieces_root`.
- `magpie-bt-metainfo`: fixture corpus generated by synthetic builders in `tests/common/mod.rs` (v1 single, v1 multi, v2 single, hybrid); five integration tests assert the correct `InfoHash` variant for each (gate #1).
- `magpie-bt-metainfo`: proptest suite + fuzz target wired to `parse` with SHA re-hash assertion; seed corpus for v1/v2 committed.
- `magpie-bt-metainfo`: criterion benchmarks (`v1_single`, `v1_multi`, `v2_single`) with baseline in `benches/BASELINE.md`.
- Workspace: added `sha1` and `sha2` as workspace-level dependencies (RustCrypto, pure Rust).

### Added (M0 phase A — bencode, post-review refinements)
- `magpie-bt-bencode`: `skip_value` / `skip_value_with` walk a value without materialising an AST, returning its byte `Range` in the input. Enables zero-alloc info-hash computation.
- `magpie-bt-bencode`: `dict_value_span` — given a top-level dict, returns the byte span of the value for a given key (ideal for locating the `info` dict in a .torrent).
- `magpie-bt-bencode`: regression tests for `i-e`, `i--1e`, `ie` and span-matches-decode invariants.
- `magpie-bt-bencode`: README documents the strict-by-design divergence from libtorrent-rasterbar.

### Added (M0 phase A — bencode)
- `magpie-bt-bencode`: zero-copy `decode`/`decode_with`/`decode_prefix` producing a borrowed `Value<'a>` AST. Strict canonical semantics (unsorted keys, duplicate keys, leading-zero integers, `-0`, and excessive nesting are rejected). Configurable `DecodeOptions { max_depth }` with `DEFAULT_MAX_DEPTH = 256`.
- `magpie-bt-bencode`: canonical `encode`/`encode_into` emitting BTreeMap-sorted dict keys and minimal integer syntax.
- `magpie-bt-bencode`: typed `DecodeError { offset, kind }` with a non-exhaustive `DecodeErrorKind`.
- `magpie-bt-bencode`: proptest suite (`encode(decode(x)) == x`, random-bytes no-panic), fuzz target wired to `decode` + `encode` round-trip, seed corpus under `fuzz/corpus/bencode/`.
- `magpie-bt-bencode`: criterion benchmarks (`small_dict`, `flat_list/{16,256,4096}`, `metainfo_like`) with baseline captured in `benches/BASELINE.md`.

### Added (M0 workspace init)
- Cargo workspace at repo root with five member crates under `crates/`: `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`, `magpie-bt-core`, `magpie-bt`. Edition 2024, MSRV 1.94, dual Apache-2.0 OR MIT.
- Workspace lint block (`missing_docs`, `unreachable_pub`, `clippy::pedantic`, `clippy::nursery`, `undocumented_unsafe_blocks`).
- `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`, `magpie-bt` use `#![forbid(unsafe_code)]`. `magpie-bt-core` uses `#![deny(unsafe_code)]` per the DISCIPLINES.md allowlist.
- Criterion bench skeletons: `bencode/benches/decode.rs`, `metainfo/benches/parse.rs`, `core/benches/{picker,alert_ring}.rs`.
- Cucumber (BDD) harness on the `magpie-bt` facade: `tests/cucumber.rs` + `tests/features/bep-0003-core.feature` + `tests/steps/mod.rs`.
- BEP coverage matrix at [`docs/bep-coverage.md`](docs/bep-coverage.md).
- DISCIPLINES.md testing table now includes the BDD row.
