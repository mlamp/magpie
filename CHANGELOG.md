# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial documentation scaffold: `README.md`, `CLAUDE.md`, `docs/PROJECT.md`, `docs/ROADMAP.md`, `docs/MILESTONES.md`, `docs/milestones/001-foundations.md`.
- Quality disciplines baseline: `docs/DISCIPLINES.md`, `docs/adr/` scaffold, GitHub CI + nightly workflows, rustfmt/clippy/deny configs, security + contributing policies.
- Dual `Apache-2.0 OR MIT` licensing.

### Changed
- `rustfmt.toml`: edition `2021` → `2024`.
- `clippy.toml`: MSRV placeholder `1.75` → `1.94` (matches latest stable on dev machine, rustc 1.94.1).
- M0 status moved `planned` → `in-progress`.
- ADRs 0001/0002/0003 accepted. ADR 0002 reworked around a custom rasterbar-style alert ring (see [docs/adr/0002-event-bus-alert-ring.md](docs/adr/0002-event-bus-alert-ring.md)).

### Added (M0 workspace init)
- Cargo workspace at repo root with five member crates under `crates/`: `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`, `magpie-bt-core`, `magpie-bt`. Edition 2024, MSRV 1.94, dual Apache-2.0 OR MIT.
- Workspace lint block (`missing_docs`, `unreachable_pub`, `clippy::pedantic`, `clippy::nursery`, `undocumented_unsafe_blocks`).
- `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`, `magpie-bt` use `#![forbid(unsafe_code)]`. `magpie-bt-core` uses `#![deny(unsafe_code)]` per the DISCIPLINES.md allowlist.
- Criterion bench skeletons: `bencode/benches/decode.rs`, `metainfo/benches/parse.rs`, `core/benches/{picker,alert_ring}.rs`.
- Cucumber (BDD) harness on the `magpie-bt` facade: `tests/cucumber.rs` + `tests/features/bep-0003-core.feature` + `tests/steps/mod.rs`.
- BEP coverage matrix at [`docs/bep-coverage.md`](docs/bep-coverage.md).
- DISCIPLINES.md testing table now includes the BDD row.
