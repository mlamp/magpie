# magpie — Roadmap

Sequenced phases from "no code" to "production-grade general-purpose BitTorrent library". For *why* each phase exists, see [PROJECT.md](PROJECT.md). For current status and per-milestone detail, see [MILESTONES.md](MILESTONES.md).

Each milestone is end-to-end shippable on magpie's own artifacts. magpie is a general-purpose library: consumer integrations (lightorrent is the current reference consumer) happen in those repos, on their timelines, and are **not** milestone gates here.

## Phases

### M0 — Foundations (no network)
Crate skeleton + workspace. Bencode encode/decode (zero-copy where possible). Metainfo parsing for v1, v2, hybrid; `InfoHash::{V1, V2, Hybrid}`. `Storage` trait + file-backed impl with vectorised I/O. Piece picker skeleton (rarest-first + endgame). Event bus: `broadcast<TorrentEvent>`. Azureus-style peer-ID builder.
**Gate**: parse real v1/v2/hybrid .torrents; storage round-trips blocks; picker sane on synthetic bitfields.

### M1 — Leecher, TCP, v1
HTTP tracker announce (BEP 3, compact BEP 23). Peer wire: handshake, bitfield, have, request/piece/cancel, choke/unchoke, interested. BEP 6 Fast extension. Download one torrent end-to-end; all hashes verify. Tracing + optional Prometheus metrics.
**Gate**: fetch Ubuntu ISO from public trackers, no leaks under `dhat`.

### M2 — Seeder + multi-torrent (consumer-integration ready)
Upload side; choking algorithm (tit-for-tat + optimistic unchoke). Multi-torrent engine, shared bandwidth limits. Persistent stats (event-driven, no polling). UDP tracker (BEP 15). Multi-tracker (BEP 12), private flag honoured (BEP 27). Public API surface audited against realistic client call-site patterns (client-agnostic). Interop verified in CI against qBittorrent + Transmission via local tracker + synthetic fixtures.
**Gate**: controlled-swarm reseed (magpie-only, synthetic ~5 MiB, SHA-256 match); 24 h ≥8-torrent soak incl. ≥100k-piece torrent; stats persist across restart (subprocess test); interop scenarios green both directions.

### M3 — Magnet + DHT
BEP 9/10 extension protocol + `ut_metadata` → magnet support. BEP 5 Kademlia DHT (study anacrolix's `dht`). BEP 11 PEX, BEP 14 LSD.
**Gate**: magnet add works, metadata fetched from peers, swarm found without tracker.

### M4 — uTP + BEP 52 hybrid
Userspace uTP (design our own after reading rakshasa, rasterbar, librqbit-utp). Full BEP 52: merkle hash verification, hybrid bi-mode, merkle layer fetch from peers.
**Gate**: download a hybrid torrent via uTP, v2 hashes verify, reseed it.

### M5 — Parity + client-replacement readiness
WebSeed (BEP 19), tracker scrape (BEP 48). UPnP/NAT-PMP (optional subcrate). Piece picker upgrade: rasterbar-style speed-class affinity. Capability bar: magpie is ready to fully replace librqbit in a production client (lightorrent is the current reference consumer — cutover happens on lightorrent's timeline, not magpie's).
**Gate**: 30-day production-grade soak demonstrated via a real consumer deployment. Memory steady, no hash failures.

### M6+ — Polish
Streaming (sequential/priority pieces). Super-seeding. SSL torrents. Pluggable storage: mmap, sqlite, S3.

## Reading order before M0

Research pass (tracked separately) must land findings in `docs/research/` for these reference implementations, in order:

1. `cratetorrent` (small, literate Rust engine).
2. `anacrolix/torrent`: `torrent.go`, `peerconn.go`, `storage/`.
3. `libtorrent-rasterbar`: `piece_picker.cpp`, `alert.hpp`, `disk_io_thread.cpp`.
4. `librqbit`: session lifecycle. (Note: `librqbit-utp` does not exist in the current tree — uTP study goes to rakshasa + rasterbar. See [research/SUMMARY.md](research/SUMMARY.md).)
5. `MonoTorrent`: `PieceHashesV2`, `TorrentManager` public API.
6. `lambdaclass/libtorrent-rs`: originally listed as a Rust v2 reference — **but this repo is v1-only** per its own README. Retain for Rust bencode/metainfo shape, not v2. See [research/SUMMARY.md](research/SUMMARY.md).

## Open items

- Register `magpie-bt` placeholder on crates.io before M0 to prevent squatting.
- Decide whether `magpie-bt-dht` and `magpie-bt-utp` are subcrates-from-day-one or features of `magpie-bt-core` (lean toward subcrates for compile-time isolation).
- Pick a benchmark harness early (custom swarm in CI vs. testing against public torrents).
