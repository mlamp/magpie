# 0017 — Upload request flow

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: anacrolix `peerRequests` split (research/002), libtorrent-rasterbar `send_buffer_watermark` (research/003 §3)

## Context

M1 was download-only: peers sent `Request` to the remote and received `Piece` back. For M2 seeding, the flow reverses — peers receive `Request` from the remote, read the block from storage, and send `Piece` on the wire. Three constraints interact:

1. **Peer task must never block on disk.** Architecture principle from `PROJECT.md`; same rationale as ADR-0007 on the download side. A 16 ms `pread` that blocks the peer task stalls every other wire event for that peer.
2. **Per-peer request queue must be bounded.** An adversarial peer can sit there pipelining 1000 `Request` messages. Unbounded queues are an OOM vector (ADR-0007 precedent).
3. **Disk reads must be rate-limited to demand.** Submitting reads as fast as `Request` messages arrive either floods the disk queue or needs disk-pool priority logic. ADR-0007 rejected priority logic in favour of rasterbar v2's "control read demand at the peer level" shape.

The cratetorrent, anacrolix, and rasterbar upload paths all converge on the same structure: two per-peer queues separating "requests we've accepted but haven't read yet" from "blocks ready to send".

## Decision

Each peer task maintains a per-peer request pipeline with two bounded stages:

```
wire Request  →  unread queue  →  [DiskOp::Read]  →  ready queue  →  wire Piece
            (cap = 128, configurable)          (gated by adaptive
                                                send-buffer watermark)
```

### New messages

`PeerToSession` gains four variants (upload side of ADR-0009's state machine):

- `Interested { slot }`, `NotInterested { slot }` — peer's interest toward us.
- `Request { slot, req: BlockRequest }` — peer wants a block.
- `CancelRequest { slot, req: BlockRequest }` — peer cancels.

`SessionToPeer` gains three:

- `SetChoking(bool)` — choker decision (ADR-0012 will use this).
- `ServeBlock { req, block: Arc<Block> }` — serve a block (fan-out-friendly; see ADR-0018).
- `RejectRequest(BlockRequest)` — BEP 6 reject (Fast ext) or "we don't have this piece".

`DiskOp` gains `Read { piece, offset, length, completion_tx }`. Completion posts `Arc<Block>` back.

### The unread queue (capacity 128 per peer, configurable)

When a `Request` arrives on the wire, the peer task:

1. **If we're choking this peer** and the piece is not in its allowed-fast set (BEP 6):
   - Within 2 s of the most recent `SetChoking(true)` sent to this peer: reject the request (`RejectRequest`) but leave the connection up. This is the rasterbar grace window for the race where the peer hasn't yet seen our `Choke`. Default 2 s, configurable.
   - After 2 s of continued choked-requests: disconnect with a protocol-violation alert. A well-behaved peer must stop requesting once it processes our `Choke`.
2. **If the request hits the allowed-fast set while choked**, track a per-peer `fast_set_requests_while_choked` counter. If the counter exceeds `3 × blocks_per_piece`, disconnect — the peer is abusing the fast set as an unlimited upload channel. Rasterbar `allowed_fast` abuse protection.
3. Drop + `RejectRequest` if we don't have the piece (partial-seed / post-drop edge case).
4. Otherwise append the `BlockRequest` to its local `VecDeque<BlockRequest>` (the *unread queue*), capped at 128 (`max_allowed_in_request_queue`, rasterbar lineage — their default is 500; magpie starts at 128 as a middle ground between throughput-to-fast-peer and per-peer memory).
5. **On overflow (queue full): reject the *incoming* request, do not drop existing queue entries.** Rasterbar shape: "the last request will be dropped". Dropping the oldest would punish the peer's most-progressed work and complicates the peer's retry logic.

### The ready queue + adaptive send-buffer watermark (pull-model read submission)

The peer task only emits `DiskOp::Read` **when its outbound socket send-buffer depth drops below an adaptive watermark**:

```text
watermark_bytes = clamp(
    (peer_upload_rate_bps * WATERMARK_HORIZON) / 8,
    MIN_SEND_BUFFER_WATERMARK,  // 128 KiB floor
    MAX_SEND_BUFFER_WATERMARK,  // 4 MiB ceiling
)
```

Defaults: `MIN = 128 KiB`, `MAX = 4 MiB`, `WATERMARK_HORIZON = 0.5 s`.

- A slow DSL peer stays at the 128 KiB floor.
- A 100 Mbps peer: `100_000_000 × 0.5 / 8 = 6.25 MiB` → clamped to 4 MiB.
- A 1 Gbps peer would compute `62.5 MiB` without a ceiling → clamped to 4 MiB.

**The ceiling is load-bearing for session memory bounds.** At 10 unchoked fast peers, an uncapped formula would give 625 MiB of ready-queue working set per torrent (`Bytes` slices from the read cache; underlying data is refcounted, but the session-wide accounting picture is still dominated by these references). Clamped to 4 MiB × 10 peers = 40 MiB worst case, which sits comfortably alongside the 64 MiB read cache (ADR-0018) and 64 MiB write budget (ADR-0007).

4 MiB at 1 Gbps covers ~32 ms of send-buffer runway — enough to absorb any realistic NVMe disk-read latency without stalling the send buffer, which is the only reason to raise the watermark above the floor in the first place. Operators running on slow spinning disks or under unusual I/O contention can raise `MAX_SEND_BUFFER_WATERMARK`; magpie's default assumes modern storage.

Rasterbar's `send_buffer_watermark` + `send_buffer_watermark_factor` shape, simplified to a floor-and-ceiling clamp: Arvid's own documentation notes the send buffer must never stall on disk-read latency, and the per-peer upload rate tells you how much runway is enough — but only up to the point where the runway exceeds realistic disk latency, at which point extra buffer is memory overhead without throughput benefit.

`peer_upload_rate_bps` is the same 20 s-windowed EWMA the SeedChoker consumes (ADR-0012). One signal, two consumers.

This is the rasterbar `send_buffer_watermark` shape: reads are pulled by send-buffer drain, not pushed by request arrival. Consequences:

- Disk queue submission rate naturally matches network drain rate. No priority logic in the disk pool (ADR-0007 keeps its FIFO).
- A slow peer doesn't consume disk budget: tiny watermark, small working set.
- A fast peer on a fat link doesn't stall on disk latency: watermark scales with its drain rate, so reads arrive before the send buffer empties.
- An adversarial peer pipelining 1000 requests can't DoS the disk — its unread queue caps at 128, and reads only flow when its send buffer actually drains.

The peer task submits one `DiskOp::Read` per drained block, with `completion_tx: mpsc::Sender<ReadCompletion>` scoped to that peer. Completions return `Arc<Block>` (see ADR-0018 — shared fan-out when multiple peers request the same block) plus the originating `BlockRequest` for correlation.

### Cancel handling

A `CancelRequest` message:

- Tries to remove the matching `BlockRequest` from the unread queue. Success → no disk work done, done.
- Otherwise the read is already in flight. Set a `cancelled` bit on the pending completion; on arrival, drop the `Arc<Block>` (refcount still decrements), do not send.

We never send a `RejectRequest` in response to our own `CancelRequest` receipt — the remote already knows.

## Consequences

Positive:

- **Peer task never blocks on disk.** Disk reads happen through `DiskWriter` just like writes; the peer task's `select!` drains `SessionToPeer` + `ReadCompletion` + wire I/O without ever doing `pread` inline.
- **Per-peer DoS bounded.** Unread queue cap 128, fast-set abuse cap 3× blocks_per_piece, post-choke 2 s grace before disconnect. No unbounded heap growth regardless of peer behaviour.
- **Natural disk-demand rate-limiting.** No priority logic, no global coordinator — the send-buffer watermark on each peer is self-pacing.
- **Fan-out-friendly.** `Arc<Block>` in `ServeBlock` means N peers requesting the same block share one `pread` (ADR-0018). At popular-torrent seeding throughput, this is the difference between 20 `pread`s per hot piece and 1.
- **Consistent with ADR-0007.** Disk pool stays strict FIFO; reads and writes share the same queue, same backpressure, same backend pool.

Negative:

- One more message type on each direction, one new `DiskOp` variant. Modest surface expansion in `session::messages` and `session::disk`.
- Per-peer `VecDeque`s cost up to `128 * sizeof(BlockRequest)` (~2 KiB) plus the dynamic ready-queue working set, which the `MAX_SEND_BUFFER_WATERMARK` clamp bounds at 4 MiB per peer. Worst case at 10 unchoked peers per torrent: 40 MiB of `Bytes` references, sharing data with the 64 MiB read cache via refcount. Slow peers are ≪1 MiB.
- The send-buffer watermark requires the peer task to know its tokio `TcpStream` write-buffer depth. This isn't exposed by tokio; we approximate with our own "bytes queued to the framed sink, not yet flushed" counter maintained alongside each `sink.send().await`. Documented as an approximation, not OS-level `SO_SNDBUF` inspection.

Neutral:

- `CancelRequest` cannot recall a read already completed on disk. The `Arc<Block>` may be dropped unsent; that's fine. Rare, cheap.

## Alternatives considered

- **Push model: submit `DiskOp::Read` on every wire `Request`.** Rejected: floods the disk queue when peers pipeline, forces priority logic in the disk pool (the 2:1 read-over-write ratio the original M2 plan proposed), which ADR-0007 explicitly rejected in favour of the rasterbar-v2 FIFO + peer-level watermark shape.
- **Single combined queue, no unread/ready split.** Rejected: conflates "accepted, awaiting disk" with "disk-done, awaiting wire"; makes cancel logic muddier (have we read it yet?) and the send-buffer watermark check loses its natural gating point.
- **Drop-oldest on unread-queue overflow.** Original ADR draft; rejected after reading rasterbar's `max_allowed_in_request_queue` code path and the "the last request will be dropped" docs. Dropping oldest punishes the peer's most-progressed request (it's been queued longest → most likely the next one we'd have served) and complicates peer-side retry bookkeeping. Drop-newest is simpler, rasterbar-aligned, and spec-legal under both BEP 3 and BEP 6.
- **Fixed send-buffer watermark.** Original draft used a fixed 128 KiB; rejected after revisiting Arvid's own guidance on watermark scaling. A fixed value is too small for fast peers (send buffer drains faster than disk can refill) and wastefully large for slow peers. Adaptive watermark = `clamp(rate × 0.5 s, 128 KiB, 4 MiB)` is the simplest form that solves both ends.
- **Uncapped adaptive watermark** (no `MAX`). Considered during the adaptive revision; rejected during final review. A 1 Gbps peer would compute 62.5 MiB of working set, and 10 such peers on a seeding hot torrent would blow past the session read-cache budget in ready-queue references alone. 4 MiB at 1 Gbps covers ~32 ms of runway — more than any realistic NVMe disk-read round-trip — so extra buffer beyond that is pure memory overhead. Floor + ceiling is the bounded shape.
- **Unread queue cap of 16.** Original draft; rejected — rasterbar defaults to 500, and the cap is the key ceiling on per-peer upload throughput ("the higher this is, the faster upload speeds the client can get to a single peer"). 16 would stall a fat-link peer constantly. 128 is the compromise between memory (~2 KiB) and throughput headroom.
- **Per-peer disk pool slots / semaphore.** Rejected: the send-buffer watermark + unread queue already bounds per-peer disk concurrency implicitly. A semaphore adds a synchronization primitive to tune without changing behaviour.
