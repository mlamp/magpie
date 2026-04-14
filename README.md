# magpie

A lean, tokio-based Rust BitTorrent library, designed from a consumer's perspective.

**Status**: M0 in progress. Bencode + metainfo shipped; core primitives (peer-ID, alert ring, storage, picker) shipped in Phase C. Unix only (Linux + macOS) for the M0 timeframe — Windows support is post-M0.

- [docs/PROJECT.md](docs/PROJECT.md) — why this project exists, architecture principles, BEP strategy.
- [docs/ROADMAP.md](docs/ROADMAP.md) — sequenced phases M0 → M6.
- [docs/MILESTONES.md](docs/MILESTONES.md) — current milestone status.
- [CLAUDE.md](CLAUDE.md) — ways of working (also relevant for human contributors).

## Crate prefix

Published on crates.io under `magpie-bt-*` (the bare `magpie` name is taken by an unrelated Othello library).

## Reachability (M2)

`Engine::listen` accepts inbound BitTorrent TCP connections and routes them to the registered torrent by `info_hash`. M2 works on LAN or with a **manually forwarded port** — automatic port-mapping via UPnP / NAT-PMP is deferred to M5. If you are behind a NAT without a forwarded port, magpie can still download: outbound connections to other peers work normally, but remote peers won't be able to initiate to you, so seeding reach is limited.
