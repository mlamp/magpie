# magpie — Milestones

Status tracker. For the narrative scope of each phase, see [ROADMAP.md](ROADMAP.md).

## Status vocabulary

- `not-started` — no detail file, not yet scoped.
- `planned` — detail file exists under `milestones/`, scope frozen, ready to start.
- `in-progress` — work underway on this milestone.
- `done` — gate criteria met and verified.

A detail file is created when a milestone transitions `not-started → planned`, using [`milestones/_template.md`](milestones/_template.md).

**Every** milestone's gate criteria include the bars in [`DISCIPLINES.md`](DISCIPLINES.md) — tests, fuzz, benchmarks, docs, CHANGELOG, and ADRs for non-trivial decisions. Milestone-specific criteria are additions on top.

## Index

| ID | Name | Status | Detail |
|----|------|--------|--------|
| M0 | Foundations (no network) | done | [000-foundations.md](milestones/000-foundations.md) |
| M1 | Leecher, TCP, v1 | done | [001-leecher-tcp-v1.md](milestones/001-leecher-tcp-v1.md) |
| M2 | Seeder + multi-torrent (consumer-integration ready) | done | [002-seeder-multi-torrent.md](milestones/002-seeder-multi-torrent.md) |
| M3 | Extension protocol + Magnet + PEX + LSD | done | [003-extension-magnet.md](milestones/003-extension-magnet.md) |
| M4 | DHT | done | [004-dht.md](milestones/004-dht.md) |
| M5 | uTP + BEP 52 hybrid | not-started | — |
| M6 | Parity + replace librqbit | not-started | — |
| M7 | Polish | not-started | — |
