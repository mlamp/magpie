# 0003 — Tokio-only runtime

- **Status**: proposed
- **Date**: 2026-04-13
- **Deciders**: TBD (confirm at M0 kickoff)

## Context

Rust async ecosystem has multiple runtimes (tokio, smol/async-std, glommio, monoio). Supporting more than one in a network-heavy library pushes complexity into every I/O primitive and every timer.

`docs/PROJECT.md` declares magpie **tokio-only**. This ADR records the reasoning and the revisit trigger.

## Decision

<!-- Commit to tokio-only. Revisit only if a concrete benchmark shows a single-runtime design hurts a real consumer's workload. -->

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
