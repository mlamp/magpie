# 0001 — Subcrate vs. feature for DHT and uTP

- **Status**: proposed
- **Date**: 2026-04-13
- **Deciders**: TBD (resolve before M3 kickoff)

## Context

`magpie-bt-core` will eventually need DHT (BEP 5, M3) and uTP (M4). Both are large, independent protocols. Two shapes are plausible:

1. **Subcrates-from-day-one**: `magpie-bt-dht` and `magpie-bt-utp` as workspace members, depended on from `magpie-bt-core` behind `[features]` flags that pull them in.
2. **Features of `magpie-bt-core`**: keep everything in one crate, gated by `#[cfg(feature = "dht")]` etc.

## Decision

**Subcrates from day one.** Ship `magpie-bt-dht` (M3) and `magpie-bt-utp` (M4) as workspace members. `magpie-bt-core` depends on each only through features (`dht`, `utp`) that pull in the corresponding subcrate. The `magpie-bt` facade re-exports the user-facing types from whichever subcrates are enabled.

## Research findings

See [docs/research/SUMMARY.md](../research/SUMMARY.md) §"ADR 0001". Key inputs:
- rasterbar ([003](../research/003-libtorrent-rasterbar.md)) keeps DHT and uTP functionally isolated at a layer boundary.
- anacrolix ([002](../research/002-anacrolix-torrent.md)) separates DHT into its own package (`anacrolix/dht` is a sister repo) — cleaner than embedding.
- librqbit's current tree ([004](../research/004-librqbit.md)) has **no uTP at all** — correction needed in PROJECT.md inspiration table. The absence itself underlines the cost of not designing for isolation up front.

## Consequences

Positive:
- Small builds stay small. Consumers who don't need DHT or uTP don't compile or link those trees.
- Compile-time isolation prevents accidental cross-cutting coupling into the core engine.
- Each subcrate can ship its own MSRV, fuzz targets, and benchmark suite without polluting the core.
- Publishing cadence decoupled: a DHT fix can release without a core version bump.

Negative:
- Slightly more workspace overhead (extra `Cargo.toml`s, five crates grow to seven eventually).
- Cross-crate visibility requires `pub` API surface; can't use `pub(crate)` convenience.
- Consumer feature-flag matrix grows.

Neutral:
- Internal refactoring across the DHT↔core boundary requires cross-crate PRs.

## Alternatives considered

- **Features in `magpie-bt-core`**: simpler, but pulls all transitive dependencies into every build regardless of feature set (cargo does not split dependency trees by feature cleanly in all cases). Rejected.
- **Optional dependencies on the facade only** (bypassing `-core`): loses the ability for `-core` to use DHT-provided types in its own API. Rejected.

## Alternatives considered

- Subcrates with re-exports through `magpie-bt-core`.
- Features inside `magpie-bt-core`.
- Optional dependencies pulled in by the `magpie-bt` facade directly (bypassing `magpie-bt-core` features).
