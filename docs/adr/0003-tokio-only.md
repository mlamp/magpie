# 0003 — Tokio-only runtime

- **Status**: accepted
- **Date**: 2026-04-13
- **Deciders**: TBD (confirm at M0 kickoff)

## Context

Rust async ecosystem has multiple runtimes (tokio, smol/async-std, glommio, monoio). Supporting more than one in a network-heavy library pushes complexity into every I/O primitive and every timer.

`docs/PROJECT.md` declares magpie **tokio-only**. This ADR records the reasoning and the revisit trigger.

## Decision

**Tokio-only.** All async primitives, timers, I/O, and task scheduling go through tokio. Revisit only if a concrete benchmark against a real consumer workload shows the single-runtime choice hurts.

## Research findings

See [docs/research/SUMMARY.md](../research/SUMMARY.md) §"ADR 0003". Key inputs:
- cratetorrent ([001](../research/001-cratetorrent.md) §7) is tokio-only; no documented pain from the choice.
- librqbit ([004](../research/004-librqbit.md) §1) is tokio-only; `CancellationToken` + `DropGuard` give a clean cascade-cancellation pattern that we will borrow.
- anacrolix is Go, rasterbar is C++ — neither has a runtime choice directly comparable.
- Our only consumer (lightorrent) is tokio-based; runtime-agnostic design would have zero payoff.

## Consequences

Positive:
- Single source of truth for timers, I/O, task scheduling.
- Consumers (lightorrent is already tokio-based) see zero friction.
- Smaller API surface; fewer abstraction layers.

Negative:
- Not usable from `async-std`/`smol`-only consumers without a runtime bridge.
- Couples us to tokio's release cadence and breaking changes.

## Alternatives considered

- Runtime-agnostic via `async-io` / `futures-io`. Cost: every I/O wrapper duplicated, every timer abstracted, cross-runtime testing burden.
- Monoio/glommio for thread-per-core. Cost: nonstandard ecosystem, limited platform support.
