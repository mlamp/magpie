# 0010 — Request pipelining + endgame

- **Status**: accepted (M1 baseline); BDP ramp + endgame deferred to Phase 4/5
- **Date**: 2026-04-13
- **Deciders**: magpie maintainers

## Context

Each peer connection has a per-side window of in-flight `Request`s. The
window depth controls throughput — too small and we leave bandwidth on the
table, too large and we waste requests when peers churn or pieces land out of
order. Cratetorrent's research notes recommend a BDP-driven ramp
(`Q ≈ B·D / 16 KiB`); rasterbar uses a fixed default plus per-peer
multipliers. We need a pragmatic M1 baseline plus a path forward.

## Decision

**M1 baseline (this milestone)**:

- Fixed per-peer in-flight ceiling: **4 requests** (`PeerConfig::max_in_flight`,
  matched by `TorrentSession::DEFAULT_PER_PEER_IN_FLIGHT`).
- `PeerConn` enforces this locally — additional `Request` commands beyond the
  cap are silently dropped (the session is responsible for not over-issuing).
- Block size is fixed at **16 KiB** (`magpie_bt_wire::BLOCK_SIZE`), enforced
  on decode (W2 hardening: `Piece` payloads larger than `8 + BLOCK_SIZE` are
  rejected by the codec).
- Scheduling is **greedy round-robin**: after every event the session walks
  peers and assigns one block at a time. Block claims (`InProgressPiece::claimed`)
  prevent two peers from being asked for the same block.
- **No endgame mode** in M1 — when only a few blocks remain, we don't
  duplicate-request them across peers. Practical impact on a 4-peer swarm is
  negligible; on real swarms it matters and is the first thing to add.
- **No cancellation on dup-arrival**: if endgame later issues the same block
  to multiple peers, `Cancel` propagation is already plumbed through
  `SessionToPeer::Cancel`.

**Phase 4 (next)**: replace the fixed `max_in_flight = 4` with cratetorrent's
slow-start formula:

```
target_in_flight = max(2, ceil(peer_throughput * rtt_estimate / BLOCK_SIZE))
```

Throughput sampled in a sliding 5-second window per peer; RTT measured from
`Request` send to first byte of `Piece` arrival. Cap at 256 to bound per-peer
memory.

**Phase 5 (end-to-end)**: enable endgame when `picker.in_endgame()` returns
true. Issue every still-missing block to every interested peer; cancel
on first arrival.

## Consequences

Positive:

- M1 baseline is simple enough to be obviously correct — the duplex
  integration test (`tests/session_duplex.rs`) exercises the entire path.
- Block claim tracking gives endgame a clean attach point — flip a mode flag
  and skip the "unclaimed" check in `assign_one_block`.
- Per-peer in-flight cap directly bounds memory: 4 requests × 16 KiB =
  64 KiB per peer.

Negative:

- Static depth = 4 leaves throughput on the table over a fat pipe to a fast
  peer. Real torrents see 2-4× speedup once BDP-tuned.
- No fairness across peers — first to be polled wins. Acceptable while we
  only run a 1-3 peer M1 leecher; revisit when M2 multi-torrent scheduling
  forces a per-torrent bandwidth budget.

## Alternatives considered

- BDP slow-start in M1 — rejected as scope creep. The measurement plumbing
  (RTT clock, throughput counter) is non-trivial and not on the critical
  path for a working leecher.
- LEDBAT-style window control — rejected as out-of-scope without uTP (M4).
- Per-piece scheduling (claim entire piece to one peer) — rejected.
  Block-level claim with fall-back to alternative peers handles peer churn
  better than piece-level.
