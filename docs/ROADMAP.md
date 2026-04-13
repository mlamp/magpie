# magpie — Roadmap

Sequenced phases from "no code" to "replace librqbit in lightorrent". For *why* each phase exists, see [PROJECT.md](PROJECT.md). For current status and per-milestone detail, see [MILESTONES.md](MILESTONES.md).

Each milestone is end-to-end shippable. Lightorrent dogfoods from **M2** behind `--engine=magpie`; librqbit stays default until **M5**.

## Phases

### M0 — Foundations (no network)
Crate skeleton + workspace. Bencode encode/decode (zero-copy where possible). Metainfo parsing for v1, v2, hybrid; `InfoHash::{V1, V2, Hybrid}`. `Storage` trait + file-backed impl with vectorised I/O. Piece picker skeleton (rarest-first + endgame). Event bus: `broadcast<TorrentEvent>`. Azureus-style peer-ID builder.
**Gate**: parse real v1/v2/hybrid .torrents; storage round-trips blocks; picker sane on synthetic bitfields.

### M1 — Leecher, TCP, v1
HTTP tracker announce (BEP 3, compact BEP 23). Peer wire: handshake, bitfield, have, request/piece/cancel, choke/unchoke, interested. BEP 6 Fast extension. Download one torrent end-to-end; all hashes verify. Tracing + optional Prometheus metrics.
**Gate**: fetch Ubuntu ISO from public trackers, no leaks under `dhat`.

### M2 — Seeder + multi-torrent + lightorrent dogfood
Upload side; choking algorithm (tit-for-tat + optimistic unchoke). Multi-torrent engine, shared bandwidth limits. Persistent stats (event-driven, no polling) — replaces the poll loop. UDP tracker (BEP 15). Multi-tracker (BEP 12), private flag honoured (BEP 27). Lightorrent `--engine=magpie` flag; CI runs both engines side-by-side.
**Gate**: lightorrent's existing test suite passes on magpie; ratio enforcement works; stats persist across restart.

### M3 — Magnet + DHT
BEP 9/10 extension protocol + `ut_metadata` → magnet support. BEP 5 Kademlia DHT (study anacrolix's `dht`). BEP 11 PEX, BEP 14 LSD.
**Gate**: magnet add works, metadata fetched from peers, swarm found without tracker.

### M4 — uTP + BEP 52 hybrid
Userspace uTP (design our own after reading rakshasa, rasterbar, librqbit-utp). Full BEP 52: merkle hash verification, hybrid bi-mode, merkle layer fetch from peers.
**Gate**: download a hybrid torrent via uTP, v2 hashes verify, reseed it.

### M5 — Parity + replace librqbit
WebSeed (BEP 19), tracker scrape (BEP 48). UPnP/NAT-PMP (optional subcrate). Piece picker upgrade: rasterbar-style speed-class affinity. Remove librqbit from lightorrent.
**Gate**: 30-day soak on lightorrent prod with magpie only. Memory steady, no hash failures.

### M6+ — Polish
Streaming (sequential/priority pieces). Super-seeding. SSL torrents. Pluggable storage: mmap, sqlite, S3.

## Reading order before M0

Research pass (tracked separately) must land findings in `docs/research/` for these reference implementations, in order:

1. `cratetorrent` (small, literate Rust engine).
2. `anacrolix/torrent`: `torrent.go`, `peerconn.go`, `storage/`.
3. `libtorrent-rasterbar`: `piece_picker.cpp`, `alert.hpp`, `disk_io_thread.cpp`.
4. `librqbit`: session lifecycle + `librqbit-utp`.
5. `MonoTorrent`: `PieceHashesV2`, `TorrentManager` public API.
6. `lambdaclass/libtorrent-rs`: Rust v2 reference — check before freezing the v2 data model.

## Open items

- Register `magpie-bt` placeholder on crates.io before M0 to prevent squatting.
- Decide whether `magpie-bt-dht` and `magpie-bt-utp` are subcrates-from-day-one or features of `magpie-bt-core` (lean toward subcrates for compile-time isolation).
- Pick a benchmark harness early (custom swarm in CI vs. testing against public torrents).
