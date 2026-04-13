# 0001 — Subcrate vs. feature for DHT and uTP

- **Status**: proposed
- **Date**: 2026-04-13
- **Deciders**: TBD (resolve before M3 kickoff)

## Context

`magpie-bt-core` will eventually need DHT (BEP 5, M3) and uTP (M4). Both are large, independent protocols. Two shapes are plausible:

1. **Subcrates-from-day-one**: `magpie-bt-dht` and `magpie-bt-utp` as workspace members, depended on from `magpie-bt-core` behind `[features]` flags that pull them in.
2. **Features of `magpie-bt-core`**: keep everything in one crate, gated by `#[cfg(feature = "dht")]` etc.

## Decision

<!-- To be filled. Current lean in docs/PROJECT.md: subcrates, for compile-time isolation and to allow the `magpie-bt` facade to cleanly re-export. -->

## Consequences

Positive / negative / neutral, to be filled with the decision.

## Alternatives considered

- Subcrates with re-exports through `magpie-bt-core`.
- Features inside `magpie-bt-core`.
- Optional dependencies pulled in by the `magpie-bt` facade directly (bypassing `magpie-bt-core` features).
