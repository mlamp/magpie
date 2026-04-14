# 0007 — Disk-write backpressure

- **Status**: accepted
- **Date**: 2026-04-13
- **Deciders**: magpie maintainers

## Context

Pieces arrive on the wire faster than they can be SHA-1-verified and written
to spinning disk. PROJECT.md has two non-negotiables:

1. *"Disk I/O never on the runtime."* Verifying SHA-1 + `pwrite` on the
   tokio worker freezes peer-event processing and risks runtime-thread
   starvation.
2. *"Bounded MPSC for any high-volume per-block stream."* Unbounded queues
   between fast producers (peers) and slow consumers (disk) are an
   OOM vector — cratetorrent's issue #22 documents the librqbit-class
   precedent.

The phase-3 band-aid (`tokio::task::spawn_blocking` inline in
`finalise_piece`) addressed (1) but blocked the per-torrent actor for the
duration of each verify+write — no other peer event for that torrent could
make progress.

## Decision

Magpie ships a dedicated [`DiskWriter`](../../crates/magpie-bt-core/src/session/disk.rs)
task per torrent (M1) — multi-torrent sharing comes in M2.

```
+-------------+   bounded mpsc(64)   +------------+   spawn_blocking
| TorrentSess | -------------------> | DiskWriter | ------------------> sha1 + write
+-------------+    DiskOp queue      +------------+
       ^                                   |
       |       bounded mpsc(64)            |
       +---------- DiskCompletion ---------+
```

- `DiskOp::VerifyAndWrite { piece, offset, buffer, expected_hash, completion_tx }`
  is enqueued by the actor with `disk_tx.send(op).await`.
- `DiskWriter::run` pops one op at a time, runs SHA-1 + storage write inside
  `tokio::task::spawn_blocking`, then publishes a `DiskCompletion` on the
  caller-supplied channel.
- The actor's `select!` loop drains both `PeerToSession` and `DiskCompletion`
  channels; `mark_have` and `Alert::PieceCompleted` fire only once disk
  acknowledges.

**Only the `disk_tx` (forward) leg is bounded.** The `completion_tx`
(return) leg is `mpsc::unbounded_channel`. Bounding both legs deadlocks
the actor↔writer pair: actor blocks on `disk_tx.send` (forward queue full)
without polling `completion_rx`; writer blocks on `completion_tx.send`
(return queue full) without draining `disk_rx`. Outstanding completions
are naturally capped by `DEFAULT_DISK_QUEUE_CAPACITY` since the writer
emits at most one completion per op it processed, so the unbounded return
leg cannot accumulate beyond that bound. (D1 hardening, found post-Phase-4
review.)

Backpressure is end-to-end:

```
disk_tx full → torrent_actor.send().await blocks
            → torrent_actor stops draining PeerToSession
            → peer.tx_to_session.send().await blocks (S1 cap = 64)
            → peer task stops reading the wire
            → TCP receive window closes
            → upstream peer stops sending
```

No explicit `DiskPermit` semaphore is required for M1 — the bounded-channel
chain produces the same guarantee.

`DiskMetrics` (atomic counters: `pieces_written`, `bytes_written`,
`piece_verify_fail`, `io_failures`) is shared between the writer and any
observability layer the consumer wires up (Phase 6 Prometheus exporter).

The "awaiting verification" gap is closed by leaving the
[`InProgressPiece`](../../crates/magpie-bt-core/src/session/torrent.rs) entry
in the actor's `in_progress` map with its buffer moved out. `received_count
== block_count` causes the scheduler to skip the piece, preventing
re-requests during the verify window. The marker is removed on completion.

## Consequences

Positive:

- The actor never blocks on disk. A 16 MiB piece taking 40 ms to hash + write
  no longer freezes the entire torrent.
- Bounded queue caps in-flight unverified buffers at `≈ DEFAULT_DISK_QUEUE_CAPACITY × piece_length`
  (default 64 × 256 KiB = 16 MiB). Configurable per-torrent.
- Single ownership for SHA-1 and write — no duplicated logic between actor
  band-aid and writer task.
- Metrics surface naturally for the Phase 6 `metrics` exporter.

Negative:

- One additional task per torrent. M2 multi-torrent should reconsider:
  shared writer pool vs per-torrent task. Per-torrent gives clean
  observability and fault isolation; pool gives better thread/cache
  affinity. Decision deferred.
- Cloning the `Sender<DiskCompletion>` per op is one alloc per piece. Cheap
  at piece rates (~10/s on Gbit) but visible in flamegraphs.

## Alternatives considered

- **Inline `spawn_blocking`** (phase-3 band-aid): rejected. Blocks the actor
  task; doesn't solve the "actor stops processing peers" problem.
- **Separate per-piece `oneshot::Sender`** for completions: equivalent
  semantics; chose `mpsc::Sender<DiskCompletion>` for fewer allocations on
  the hot path and one-channel select.
- **`DiskPermit` semaphore** (rasterbar style): not needed in M1 — the
  bounded-channel chain already produces TCP backpressure. Revisit in M2 if
  multi-torrent sharing complicates the simple chain.
- **Direct `io_uring` / mmap**: out of scope. Phase-7+ research.
