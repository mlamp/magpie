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
|    3 | The BitTorrent Protocol                         | planned       | M1        | [bep-0003-core.feature](../crates/magpie-bt/tests/features/bep-0003-core.feature)                    | Core wire + tracker + metainfo |
|    6 | Fast Extension                                  | planned       | M1        | —                                                                                                    | Suggest, Reject, HaveAll, HaveNone, AllowedFast |
|    9 | Extension for peers to send metadata            | planned       | M3        | —                                                                                                    | `ut_metadata`; magnet support |
|   10 | Extension Protocol                              | planned       | M3        | —                                                                                                    | Extension-handshake framing |
|   12 | Multitracker metadata extension                 | planned       | M2        | —                                                                                                    | Tiered announce URLs |
|   15 | UDP Tracker Protocol                            | planned       | M2        | —                                                                                                    | |
|   23 | Tracker returns compact peer lists              | planned       | M1        | —                                                                                                    | |
|   27 | Private Torrents                                | planned       | M2        | —                                                                                                    | Honour `private` flag; suppress DHT/PEX/LSD |
|   29 | µTorrent Transport Protocol (uTP)               | planned       | M4        | —                                                                                                    | Userspace uTP |
|   52 | The BitTorrent Protocol Specification v2        | planned       | M0 / M4   | —                                                                                                    | Data model from M0; wire + merkle verification from M4 |
|    5 | DHT Protocol                                    | planned       | M3        | —                                                                                                    | Kademlia |
|   11 | Peer Exchange (PEX)                             | planned       | M3        | —                                                                                                    | `ut_pex` |
|   14 | Local Service Discovery                         | planned       | M3        | —                                                                                                    | |
|   19 | HTTP/FTP seeding (GetRight style)               | planned       | M5        | —                                                                                                    | WebSeed |
|   48 | Tracker Scrape                                  | planned       | M5        | —                                                                                                    | |

## How to update

1. When a BEP moves from `planned` → `partial` or `done`, update its row and add/link the feature file.
2. Add a short change line to `CHANGELOG.md` under `## [Unreleased]`.
3. If implementing a BEP not in the table above, add the row and link the source spec.
4. Keep the table order stable (grouped roughly by implementation wave, not BEP number) so diffs stay readable.
