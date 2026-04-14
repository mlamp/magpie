# 0020 — Peer need-set

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers

## Context

The M2 plan reserved a standalone ADR slot for peer need-set tracking (which pieces a peer needs from us) to drive the SeedChoker in ADR-0012 and to leave a hook for BEP 16 super-seeding (M6+). During the ADR-0005 (picker) review, the need-set design was folded into that ADR because it is tightly coupled to the picker's scope boundary and to `PeerState`'s bitfield representation.

This ADR exists as the canonical discoverable entry for "where does magpie track peer need-sets" — it documents the outcome and points to where the design lives.

## Decision

**Peer need-set handling is specified in [ADR-0005](0005-picker-architecture.md), under §"Peer need-set (ADR-0020 hook)".**

Summary of the decision there:

- Store the peer's advertised bitfield on `PeerState` (upgrade the existing `have: Vec<bool>` to `BitVec` for bitwise ops).
- **Compute the need-set on-demand** as `our_have & !peer.have` at each SeedChoker read (one bitfield AND per peer per 10 s tick).
- **No cached need-set.** Explicitly rejects the incremental-update cache pattern — it has a staleness trap at the leech→seed completion transition (ADR-0019) because `our_have` flips in one shot and every cached need-set would need a fix-up pass. On-demand computation sidesteps the bug entirely.
- `BitVec` for M2; `RoaringBitmap` deferred until bitfield operations show up on flamegraphs (not expected at M2 peer counts).
- **M2 SeedChoker does not consume the need-set in its ranking** (ADR-0012 ranks by upload-rate-to-peer only). The hook stays available for BEP 16 super-seeding in M6+.

## Consequences

- **No duplicate state.** The peer's advertised bitfield lives in exactly one place (`PeerState.have`); the need-set is derived, not stored.
- **No fix-up step at completion** (ADR-0019). The leech→seed transition does not need a need-set rebuild pass because there's nothing cached to invalidate.
- **Super-seeding has the data it needs** when it eventually lands. The `our_have & !peer.have` expression is the need-set; a future `SuperSeedChoker` consumes it the same way a future `SuperSeedPicker` would.
- **This ADR is a pointer, not an owner.** Updates to need-set handling happen in ADR-0005; this file stays as the search anchor.

## Alternatives considered

- **Keep ADR-0020 as the canonical owner** of need-set design (split from ADR-0005). Rejected: the design is small enough that splitting it creates two ADRs that must agree, and the picker's scope boundary is inseparable from where need-sets live. One ADR, one pointer, is less drift-prone.
- **No ADR-0020 at all**, just the content in ADR-0005. Rejected: the M2 plan references ADR-0020 by number, and future contributors searching "peer need-set ADR" will hit a dead end if no file exists. A short pointer record resolves this at ~30 lines of cost.
