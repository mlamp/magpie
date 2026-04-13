# 0002 — Event bus on `tokio::sync::broadcast`

- **Status**: proposed
- **Date**: 2026-04-13
- **Deciders**: TBD (resolve during M0)

## Context

Consumers need to observe torrent-level events (piece completed, peer connected, stats snapshot, error) without polling. The library must not backpressure the engine if a consumer is slow.

Candidates:
1. `tokio::sync::broadcast<TorrentEvent>` — lossy, slow consumers get `Lagged(n)`, engine never blocks.
2. Per-subscriber `mpsc` channels — no drops, but slow consumers can stall or force the engine to drop events anyway via `try_send`.
3. Custom ring buffer inspired by libtorrent-rasterbar's alert API — more control, more code.

`docs/PROJECT.md` currently commits to option 1. This ADR exists so the *why* is recorded and option 3 can be revisited if broadcast proves inadequate under high-volume per-block streams.

## Decision

**`tokio::sync::broadcast<TorrentEvent>` for torrent-level events, bounded `mpsc` for any high-volume per-block or per-request stream.** Bounded channel; slow consumers receive `RecvError::Lagged(n)` and are expected to call `TorrentHandle::snapshot()` to resync. The engine itself never back-pressures on event delivery.

## Research findings

See [docs/research/SUMMARY.md](../research/SUMMARY.md) §"ADR 0002". Key inputs:
- librqbit ([004](../research/004-librqbit.md) §2): confirms polling is the current-day pain — `api_stats_v1` recomputes from state on every HTTP call. This is exactly magpie's differentiator.
- anacrolix ([002](../research/002-anacrolix-torrent.md) §4, §8): synchronous callbacks hold `Client`/`Torrent` locks; slow callbacks stall the picker. Magpie's broadcast avoids this class of bug entirely.
- rasterbar ([003](../research/003-libtorrent-rasterbar.md) §1): double-buffered alert ring is the most scalable design but 10× the code we need at our scale. broadcast is the right trade-off.
- cratetorrent ([001](../research/001-cratetorrent.md) §4): unbounded mpsc alerts work for one consumer; don't scale to multiple subscribers.

## Consequences

Positive:
- Non-blocking: engine task never waits on a consumer.
- Typed `TorrentEvent` enum is statically checkable by subscribers (unlike dynamic-callback approaches).
- Multiple consumers (e.g. UI + metrics exporter + test harness) can subscribe independently.
- Late subscribers resync via explicit snapshot API — catchup is a deliberate contract, not a framework feature.

Negative:
- `Lagged` is a user-visible failure mode. Consumers must handle it correctly.
- broadcast fan-out cost scales linearly with subscribers; fine for our expected scale (≤10 subscribers per torrent).
- No persistent event history; losing `Lagged` events means the snapshot API must give enough state to reconstruct user-visible progress.

Neutral:
- Per-block streams (e.g. individual block requests from a peer) use bounded `mpsc` instead of broadcast — a separate channel class per stream kind, documented in `magpie-bt-core` API docs.

## Alternatives considered

- **Synchronous callbacks** (anacrolix, MonoTorrent): rejected — locks-under-callbacks pain documented.
- **Unbounded mpsc** (cratetorrent): rejected — one-consumer-only.
- **Custom ring buffer** (rasterbar): rejected — over-engineered for our scale; revisit only if broadcast proves inadequate.
- **No event bus, consumer polls** (librqbit): rejected — the gap we're explicitly fixing.
