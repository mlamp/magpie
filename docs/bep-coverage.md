# BEP coverage

Live matrix of BitTorrent Enhancement Proposals and magpie's implementation state.

Source index: <https://www.bittorrent.org/beps/bep_0000.html>

## Status values

- `not-started` — no implementation, no scenarios.
- `planned` — scheduled for a named milestone; may have stub scenarios.
- `partial` — some functionality landed; features exist but not exhaustive.
- `done` — fully implemented, feature file exhaustive, passes in CI.
- `deferred` — deliberately postponed past the current roadmap.

## Matrix

| BEP  | Title                                           | Status        | Milestone | Features                                                                                             | Notes |
|-----:|-------------------------------------------------|---------------|-----------|------------------------------------------------------------------------------------------------------|-------|
|    3 | The BitTorrent Protocol                         | done          | M1        | [bep-0003-core.feature](../crates/magpie-bt/tests/features/bep-0003-core.feature)                    | Core wire + tracker + metainfo |
|    6 | Fast Extension                                  | done          | M1        | [bep-0006-fast.feature](../crates/magpie-bt/tests/features/bep-0006-fast.feature)                    | HaveAll, HaveNone, AllowedFast, RejectRequest, SuggestPiece |
|    9 | Extension for peers to send metadata            | done          | M3        | [bep-0009-metadata.feature](../crates/magpie-bt/tests/features/bep-0009-metadata.feature)            | `ut_metadata` codec + assembler + magnet end-to-end |
|   10 | Extension Protocol                              | done          | M3        | [bep-0010-extension.feature](../crates/magpie-bt/tests/features/bep-0010-extension.feature)          | Extension-handshake framing + per-peer ID registry; fuzz target shipped |
|   12 | Multitracker metadata extension                 | partial       | M2        | [bep-0012-multi-tracker.feature](../crates/magpie-bt/tests/features/bep-0012-multi-tracker.feature)  | `TieredTracker` with tier fall-through + promote-on-success |
|   15 | UDP Tracker Protocol                            | partial       | M2        | [bep-0015-udp-tracker.feature](../crates/magpie-bt/tests/features/bep-0015-udp-tracker.feature)      | Codec landed; demux-driven client pending |
|   23 | Tracker returns compact peer lists              | done          | M1        | [bep-0023-compact.feature](../crates/magpie-bt/tests/features/bep-0023-compact.feature)              | v4 + BEP 7 v6 + dict-form |
|   27 | Private Torrents                                | partial       | M2        | [bep-0027-private.feature](../crates/magpie-bt/tests/features/bep-0027-private.feature)              | Flag parsed + plumbed via `TorrentParams::is_private()`; peer-discovery consumers land in M3 |
|   29 | µTorrent Transport Protocol (uTP)               | planned       | M5        | —                                                                                                    | Userspace uTP |
|   52 | The BitTorrent Protocol Specification v2        | planned       | M0 / M5   | —                                                                                                    | Data model from M0; wire + merkle verification from M5 |
|    5 | DHT Protocol                                    | planned       | M4        | —                                                                                                    | Kademlia; standalone `magpie-bt-dht` subcrate |
|   11 | Peer Exchange (PEX)                             | done          | M3        | [bep-0011-pex.feature](../crates/magpie-bt/tests/features/bep-0011-pex.feature)                      | `ut_pex` codec + diff-based outbound + inbound rate-limit + private-flag suppression |
|   14 | Local Service Discovery                         | done          | M3        | [bep-0014-lsd.feature](../crates/magpie-bt/tests/features/bep-0014-lsd.feature)                      | Multicast announce + listener + cookie self-filter + private-flag suppression |
|   19 | HTTP/FTP seeding (GetRight style)               | planned       | M6        | —                                                                                                    | WebSeed |
|   48 | Tracker Scrape                                  | planned       | M6        | —                                                                                                    | |

## How to update

1. When a BEP moves from `planned` → `partial` or `done`, update its row and add/link the feature file.
2. Add a short change line to `CHANGELOG.md` under `## [Unreleased]`.
3. If implementing a BEP not in the table above, add the row and link the source spec.
4. Keep the table order stable (grouped roughly by implementation wave, not BEP number) so diffs stay readable.
