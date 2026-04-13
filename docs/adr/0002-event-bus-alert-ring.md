# 0002 — Event bus: custom rasterbar-style alert ring

- **Status**: proposed
- **Date**: 2026-04-13 (revised)
- **Deciders**: TBD (resolve during M0)

## Context

Consumers need to observe torrent-level events (piece completed, peer connected, state change, stats tick, errors) and — for some use cases — high-frequency per-block streams, without polling. The library must never back-pressure the engine task regardless of consumer speed.

Performance constraints:

- Typical torrent-level event rate: up to ~100/s per torrent (piece completion at high throughput + peer churn + stats ticks).
- Per-block event rate at gigabit: ~8000/s per torrent.
- Expected consumers per process: 1 primary (the main application) + optional metrics/log/debug sidecars (0–3).
- Magpie is a library others will depend on; a perf regression is felt by every consumer.

Off-the-shelf candidates and their cost profiles:

1. **`tokio::sync::broadcast<TorrentEvent>`** — per-subscriber slot ring, lock per send, **every subscriber clones the event on `recv()`**. With `TorrentEvent` carrying `Vec`s or `HashMap`s, N subscribers = N deep clones per event. Cheap to wire up (~3 LoC), but deep clones scale poorly at 8000 events/s × N subscribers.
2. **`broadcast<Arc<TorrentEvent>>`** — fixes the clone-heavy payload at the cost of one heap allocation per event. Fanout is atomic bump. Good enough for low-rate torrent-level events; still pays 1 heap alloc/event.
3. **Custom rasterbar-style double-buffered alert queue** — arena-backed typed enums, zero alloc per event after arena warm-up, batch drain, explicit drop policy on overflow. ~300 LoC of infrastructure.
4. **Synchronous callbacks (anacrolix model)** — rejected; holds engine locks, slow callbacks stall the picker (cite [002 §4](../research/002-anacrolix-torrent.md)).

## Decision

**Build a custom rasterbar-style alert ring** as the primary magpie event bus. Concretely:

```
magpie-bt-core/src/alerts/
├── mod.rs           // public API: AlertQueue, AlertReader, Alert enum
├── arena.rs         // bump-arena storage per generation
├── ring.rs          // double-buffer + generation counter
└── categories.rs    // category mask (piece, peer, tracker, error, stats, ...)
```

Key properties:

- **Double-buffered storage.** Two generations (A and B). Producer writes into the "active" generation; consumer drains the "inactive" generation. `swap_generation()` is an atomic fetch-add on a 64-bit counter.
- **Arena-backed typed `Alert` enum.** Alerts are `Sized`; variants carry small payloads by value (stats tick, piece index, peer handle id). Heavy payloads (e.g. error detail strings) live in the arena via a compact `ArenaStr`-style handle — zero heap alloc per event once the arena is warm.
- **Batch drain.** `AlertReader::pop_all() -> impl Iterator<Item = &Alert>` returns everything from the inactive generation in one call. Consumer loops through once, then triggers swap for next batch. Cuts per-alert syscall/wake cost.
- **Category masking.** Consumers declare which `AlertCategory` bits they care about (`PIECE | PEER | ERROR | STATS | …`). Alerts outside the mask are not delivered (not cloned, not counted against the subscriber's buffer). Mirrors rasterbar.
- **Explicit overflow policy.** If the producer outruns the consumer and the inactive generation is still being drained, the oldest alerts are dropped and a `Alert::Dropped(n)` sentinel is enqueued. Producer never blocks. Consumer learns the exact count of lost alerts.
- **Single primary reader per torrent.** A torrent owns one `AlertQueue`; the session provides a single `AlertReader`. Multi-subscriber fan-out (app + metrics + log) is a consumer-side concern — magpie ships a small `fan_out()` helper that distributes pop'd alerts across N `tokio::sync::mpsc::UnboundedSender`s, but the hot path through magpie itself is single-reader, zero-clone.
- **Async wake.** `AlertReader::wait()` is `async`, implemented via `tokio::sync::Notify`; woken on generation swap.
- **High-frequency per-block streams** (per-request, per-have) use dedicated bounded `mpsc` channels **separate** from the alert ring, because they are typed consumer-specific, not broadcast. Those channels' backpressure *does* flow to the peer that generated them (choking the peer, not the engine).

## Research findings

See [docs/research/SUMMARY.md](../research/SUMMARY.md). Key inputs:

- **rasterbar ([003](../research/003-libtorrent-rasterbar.md) §1)**: double-buffered queue + generation counter + category masking is the reference design. Battle-tested in 20+ years of production BT clients at gigabit-plus throughput.
- **librqbit ([004](../research/004-librqbit.md) §2)**: confirms polling (no event bus) is the gap magpie must close.
- **anacrolix ([002](../research/002-anacrolix-torrent.md) §4, §8)**: synchronous callbacks under engine locks are a known failure mode to avoid.
- **cratetorrent ([001](../research/001-cratetorrent.md) §4)**: unbounded mpsc alerts work for one consumer, don't scale.

The prior revision of this ADR leaned toward `broadcast<Arc<TorrentEvent>>` for code simplicity. Superseded: the project preference is perf over code footprint; the custom ring matches the most scalable reference design we found.

## Consequences

Positive:

- **Zero heap allocation per event** in steady state (after arena warm-up). Sustains 8000+ events/s per torrent without GC pressure.
- **Batch drain** collapses N wake events into one, reducing context-switch cost when the consumer is behind.
- **Category masks** let sidecar consumers subscribe to narrow slices (e.g. metrics-only) without draining everything.
- **Explicit, typed drop sentinel** makes data loss observable instead of silent.
- **Single-reader hot path** keeps producer code trivial — no per-subscriber fanout loop inside the engine.
- **Matches rasterbar's operational profile** — decades of real-world tuning carry over.

Negative:

- **~300 LoC of infrastructure code** in `magpie-bt-core/src/alerts/` that we own, fuzz, and maintain. Larger than `broadcast<Arc<_>>` by ~100×.
- **Alerts must be `Sized` and arena-friendly.** Heavy payloads (e.g. full peer-stats snapshots) cannot be embedded in-alert; use compact IDs + a separate query API. This is a design constraint consumers must learn.
- **Multi-consumer fanout is consumer-side work** (magpie ships a helper, but it's not free like broadcast's native fanout).
- **Arena lifetime coupling.** Alerts pop'd from generation N are borrow-valid only until the next swap. Consumers that need to retain alerts past the swap must copy them out — documented in the API, enforced by Rust lifetimes.

Neutral:

- Per-block/per-request streams use bounded `mpsc`, not the alert ring.
- `cargo-fuzz` target `alert_ring` from M0 — fuzzing the ring is mandatory given we wrote it.
- Benchmarks: produce/drain throughput in `magpie-bt-core/benches/alerts.rs`, baselined in M0, guarded against ≥5% regression per DISCIPLINES.md.

## Alternatives considered

- **`tokio::sync::broadcast<TorrentEvent>`**: rejected — per-subscriber clone scales badly; `TorrentEvent` payload shape would have to be force-fit into `Copy` or all-Arc to avoid the cost.
- **`broadcast<Arc<TorrentEvent>>`**: rejected — still 1 heap alloc/event; no batch drain; no category filtering.
- **Synchronous callbacks (anacrolix/MonoTorrent)**: rejected — known to stall engine loops under slow consumers.
- **Unbounded mpsc alerts (cratetorrent)**: rejected — one-consumer-only; no backpressure policy on runaway event rates.
