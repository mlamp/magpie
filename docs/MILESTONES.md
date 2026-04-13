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
| M0 | Foundations (no network) | in-progress | [001-foundations.md](milestones/001-foundations.md) |
| M1 | Leecher, TCP, v1 | not-started | — |
| M2 | Seeder + multi-torrent + lightorrent dogfood | not-started | — |
| M3 | Magnet + DHT | not-started | — |
| M4 | uTP + BEP 52 hybrid | not-started | — |
| M5 | Parity + replace librqbit | not-started | — |
| M6 | Polish | not-started | — |
