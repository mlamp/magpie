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

<!-- Current lean: broadcast for torrent-level events, bounded mpsc for high-volume per-block streams. Fill in after M0 picker + event integration lands. -->

## Consequences

TBD.

## Alternatives considered

See Context.
