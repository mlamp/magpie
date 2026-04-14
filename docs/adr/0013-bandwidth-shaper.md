# 0013 — Bandwidth shaper

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: libtorrent-rasterbar `bandwidth_manager` (research/003), classic token-bucket literature, ADR-0017 (per-peer send-buffer watermark sets disk-read-submission pacing, distinct from wire-rate shaping)

## Context

M2 adds multi-torrent session support (workstream D), which means global and per-torrent bandwidth caps become meaningful — a user wants "limit this session to 50 MB/s up" and "limit the `*arr` torrent to 10 MB/s so my desktop stays usable." Without a shaper, magpie can saturate the link and the only lever available is per-connection TCP behaviour, which doesn't aggregate usefully across many torrents or many peers.

Three design axes:

1. **Where to enforce**: on the wire path (consume on send/recv) or upstream (pace requests / paces reads)?
2. **Tier structure**: single global cap, per-torrent caps, per-peer caps, or hierarchy?
3. **What happens when caps are disabled**: is the "unlimited" path a first-class citizen in the same mechanism, or a special-case bypass?

On (1): consume-on-wire is the only option that works symmetrically. Upload: check bucket before writing to the framed sink. Download: check bucket before reading into the peer task's inbox; when empty, stop reading, and TCP's window naturally backs off the remote. Pacing request messages on the download side doesn't bound incoming rate (peer can still send more than we requested via block size), and pacing disk reads doesn't bound outgoing rate (that's ADR-0017's send-buffer watermark, a different knob).

On (2): a single global cap can't express per-torrent limits. Per-torrent alone can't express a session-wide ceiling. Per-peer alone can't express either. Hierarchy is the only option; the question is how many tiers. Three (global / per-torrent / per-peer) is what rasterbar's `bandwidth_manager` ships, and it's the minimum set that covers real user requirements — lightorrent's *arr operators will want all three.

On (3): the M2 plan settled this explicitly: *all three tiers participate in the refill cycle from day one, with "unlimited" expressed as a bucket of capacity `u64::MAX` rather than a skip-this-tier shortcut.* Rationale there is about avoiding a mid-milestone refactor when caps are later enabled.

## Decision

### Six buckets per session

Three tiers × two directions (up, down). Each tier is a classic token bucket:

```rust
struct TokenBucket {
    tokens: AtomicU64,
    capacity: u64,         // max tokens in-hand (burst allowance)
    refill_rate: u64,      // tokens/sec (the cap)
    last_refill: AtomicI64, // Instant encoded as nanos since session start
}
```

- **Session tier** (2 buckets: up, down) on the `Session` actor.
- **Torrent tier** (2 buckets × N torrents) on each `TorrentActor`.
- **Peer tier** (2 buckets × M peers) on each peer task.

Default cap is `u64::MAX` at every tier, which is effectively unlimited. Default `capacity = u64::MAX` means "no cap" — the bucket always has tokens.

### Refill loop

One refiller task per session runs at a fixed 100 ms tick. Each tick:

1. **Gather demand**: each child reports bytes consumed since last tick + bytes denied by bucket emptiness. Reported via an `AtomicU64` on the child (demand counter), read-and-reset each tick.
2. **Parent → child grant**: each parent allocates `share = min(demand_child, parent_available)` proportionally across children whose demand exceeds their own current tokens. Proportional-to-demand, not max-min (rasterbar shape).
3. **Grant is added to child bucket** via `AtomicU64::fetch_add`, clamped to the child's `capacity`.

At the session tier, "parent" is the configured cap — refill is `cap × 0.1 s = cap/10` tokens per tick, capped to the session bucket's `capacity`. Burst capacity defaults to `1 × cap_per_second` (1 s of headroom) to absorb bursty demand.

The refill loop is O(torrents + peers) per tick. At 100 torrents × 50 peers/torrent = 5000 entries, 10 ticks/sec → 50k atomic ops/sec, invisible.

### Hot path: consume-on-wire

Upload (peer wants to send a block of `n` bytes to the framed sink):

```rust
async fn send_block(peer: &PeerState, n: u64) {
    loop {
        if peer.up_bucket.try_consume(n) {
            // framed_sink.send(...).await
            return;
        }
        peer.up_notify.notified().await;  // woken by refiller
    }
}
```

`try_consume` is a single `AtomicU64::fetch_sub` with a CAS retry if it goes below zero (restore and return false). On success the peer task proceeds immediately. On failure it awaits a `Notify` signalled by the refiller when this peer receives a new grant.

Download path is symmetric: before reading bytes off the wire, check the down-bucket; if empty, pause the read loop until notified. TCP receive window closes naturally while we're paused, backpressuring the remote.

**The session and torrent tiers are never touched on the byte path.** Only the peer bucket is checked per send/recv. Session + torrent buckets are refilled from the session cap on the 100 ms timer and drained by the refiller distributing to children. Two atomics per sent/received block, not six.

### Demand signal per child

```rust
struct DemandCounters {
    // `consumed` is NOT a dedicated atomic — see note.
    denied: AtomicU64,  // bytes denied by bucket emptiness
}
```

**`consumed` is computed by the refiller, not maintained as a separate atomic.** Per ADR-0014, each peer already owns a cumulative `AtomicU64 uploaded` / `AtomicU64 downloaded` on the byte path. The refiller stores a per-peer `prev_sample_uploaded` / `prev_sample_downloaded` and at each tick computes `consumed = current - prev_sample` before updating `prev_sample`. One atomic-add per byte-path op covers both the stats counter (ADR-0014) and the shaper's consumed-since-last-tick signal. An earlier draft had a dedicated `consumed` atomic alongside the stats counter; deleted during the ADR-0014 pass.

**`denied` tracks bytes, not attempts.** On a failed `try_consume(n)`, the call adds `n` to `denied` **exactly once** and then awaits the refill notify. Callers must not loop retrying `try_consume` in a tight spin — that would inflate `denied` by the retry count, and the proportional-allocation refiller would over-grant to peers whose tasks happen to have high wake-up frequency rather than high actual demand. The pseudocode above (single `try_consume` then `notified().await`) implements this correctly; the invariant is encoded in the counter semantics so a future contributor who adds a retry loop has to think about it.

`denied` is the starvation signal: a peer that wants more than it gets reports high denied; a peer that's comfortable reports only consumed. `demand = consumed + denied` captures "what this child would use if unshaped." `denied` is read-and-reset per tick with atomic `swap`; `consumed` is computed from the stats counter delta, not reset.

### Pass-through in the refill cycle

A torrent with no cap still has its bucket, still reports demand, still receives a grant (which is always ≥ its demand because the parent cap is also `u64::MAX`). The refill code never branches on "is this tier capped" — it just computes shares, and the math for unlimited is identical. This is the plan-level commitment: flipping on a per-torrent cap in M5 changes `refill_rate` and `capacity`, not the codepath.

### Interaction with other rate mechanisms

- **Per-peer send-buffer watermark (ADR-0017)**: paces `DiskOp::Read` submissions based on socket send-buffer depth. Orthogonal to the bandwidth shaper. Watermark controls *when* reads are submitted; shaper controls *how fast* the resulting blocks are written to the wire.
- **Choker (ADR-0012)**: selects *which* peers get to receive from us. Shaper bounds *how much* each unchoked peer receives. Choker's rate EWMA and shaper's consumed counter are independent atomics on the same `PeerState`.
- **StatsUpdate (ADR-0014)**: 1 Hz consumer-facing aggregate; does not drive the shaper or choker.

**Shaper × choker interaction, documented explicitly**: the choker's seed-mode ranking (ADR-0012) uses 20 s-EWMA of upload-rate-to-peer, which is the *shaped* throughput as observed at the wire, not the peer's potential throughput. If an operator sets a per-torrent cap below a fast peer's link capacity, the shaper throttles it, its observed rate stays low, and the choker will demote it in the next round in favour of another peer who happens to fit within the cap. This is correct behaviour given the operator's stated intent (a per-torrent cap *is* the operator saying "don't exceed this") but can be surprising: a peer who would be fastest on an uncapped pipe may not win a regular slot on a capped torrent. Operators who want "serve the fastest peer first, then limit total rate" should cap the session tier and leave the torrent uncapped; the hierarchy supports both shapes.

### Runtime cap changes

Caps are reconfigurable at runtime via `Session::set_cap(direction, tier, bytes_per_sec)` and its torrent-level equivalent. On reconfiguration:

1. The new `refill_rate` is written atomically.
2. The new `capacity` (burst allowance) is also written; default is `1 × new_rate`.
3. **Current `tokens` are clamped to `min(current_tokens, new_capacity)`**. Without this clamp, a bucket previously unlimited (`u64::MAX` tokens) would retain that balance and the new cap would effectively not enforce until the bucket drained naturally — which at 10 MB/s would take ~58 000 years. Clamp is one `AtomicU64::fetch_min`.

Operators see the new cap take effect on the next refill tick (worst case 100 ms lag) with no replay of previously-granted tokens. Used by lightorrent to honour per-category rate limits when an operator edits config at runtime.

## Consequences

Positive:

- **Two atomics per byte-path operation** (peer bucket + peer `Notify` check). Session and torrent tiers are invisible to the hot path.
- **Hierarchical enforcement is correct**: a 50 MB/s session cap + 10 MB/s per-torrent cap interact exactly as operators expect — torrent can't exceed 10, session can't exceed 50 regardless of how many torrents.
- **Pass-through bucket participates in the refill cycle**: flipping on caps in M5 is a configuration change, not a code change.
- **TCP receive-window behaves sensibly** on download cap: paused read loop closes the window; remote sees backpressure without any magpie-side buffer growth.
- **Demand signal naturally converges**: a starving child reports high `denied` and gets a larger grant next tick; a comfortable child reports low demand and doesn't monopolise parent budget.
- **Directional separation**: upload and download caps never interfere. A full upload cap doesn't slow downloads.

Negative:

- **100 ms refill tick is a latency floor for bucket starvation**. A peer emptying its bucket waits up to 100 ms for the next grant. On the download side the overshoot between "bucket empty" and "read loop paused, TCP window closed" is bounded by the kernel's `SO_RCVBUF` — typically 128–256 KiB on Linux defaults, tunable up to a few MiB but rarely larger in practice — **not** by any application-level buffer, since magpie's per-peer tasks read directly from the socket into bounded inboxes. So a cap violation is a handful of KiB per tick at the shaping moment, not megabytes. Refill can go faster (e.g. 50 ms) at the cost of more scheduler wakeups; 100 ms is the rasterbar-proven middle.
- **Proportional-to-demand is not max-min fair**. A peer with 10× the demand gets 10× the grant, but if all peers' demand exceeds the cap, the fast peer still gets the lion's share. Max-min fairness (each peer gets its demand *up to the smallest requesting peer*, then any surplus is redistributed proportionally among those who still want more) is nicer for mixed-speed swarms. Deferred: implement if the 24 h soak shows starvation of slow peers under a tight cap.
- **Six buckets per session scales linearly with (torrents × peers)**. At 100 torrents × 50 peers the refiller touches 10,000 peer buckets per tick. Still fine at 100 ms, but an operator with 1000 torrents should raise the refill interval or accept the 10× work cost.
- **Burst capacity = 1 s of cap** means a peer that has been idle for 1 s can suddenly transmit 1 full cap-second of data. By design for handshake + first-block priming; could be surprising for a strict cap. Configurable.

Neutral:

- The shaper treats TCP and (future) uTP identically — both consume bytes through the same `PeerState` counters. The M4 uTP landing doesn't need shaper changes.
- The shaper ignores overhead bytes (TCP/IP headers, framing bytes for keep-alives). Rasterbar also ignores overhead; users who want strict-to-the-wire enforcement are not magpie's target (and would need OS-level shaping anyway).

## Alternatives considered

- **Single global cap, no hierarchy.** Rejected: can't express per-torrent limits, the single most-requested feature for any multi-torrent client.
- **Per-peer buckets only, no parent aggregation.** Rejected: can't enforce a global cap correctly without coordination. N peers at (cap/N) each doesn't work when peer demand is non-uniform.
- **Four tiers (session / group / torrent / peer) with a "group" tier for batching related torrents.** Rejected for M2: no current consumer needs groupings. Lightorrent's categories would be the obvious fit, but category = torrent in its data model. Revisit if a consumer ships explicit grouping.
- **Leaky bucket instead of token bucket.** Mathematically equivalent for uncapped bursts but the token bucket's `capacity` parameter (separate from `refill_rate`) gives operators an independent burst knob that leaky bucket conflates. Token bucket is also the rasterbar shape.
- **Max-min fairness from day one.** Considered; rejected as M2 scope. Proportional-to-demand is what rasterbar ships and is observably fine for the common case. Max-min is a one-ADR upgrade if soak data shows need.
- **Skip the per-torrent tier when capped at unlimited (pure bypass).** Explicitly rejected per the M2 plan pre-flight. Introducing caps in M5 would require rewiring the refill path; keeping the tier present-but-unlimited keeps the path exercised.
- **Refill rate in bytes/ms rather than bytes/sec.** Finer-grained but operator-unfriendly — users think in MB/s, not MB/ms. Internal computation uses nanoseconds for precision, exposed config is bytes/sec.
