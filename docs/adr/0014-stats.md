# 0014 — Stats: counters, events, persistence

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: research/SUMMARY.md ("consumer state is consumer-owned"), ADR-0002 (alert ring), ADR-0012 (choker reads), ADR-0013 (shaper demand signal), lightorrent M1 `src/engine.rs` (poll-loop and baseline `HashMap<InfoHash, StatsBaseline>`)

## Context

M1 ships a partial stats story: the wire layer has per-peer byte counters, the disk path has `DiskMetrics`, but there is no cumulative up/down counter, no event, and no persistence. Lightorrent currently works around this by polling `librqbit::Session` every `ratio_poll_interval_secs` and diffing against a `baselines: Arc<Mutex<HashMap<[u8;20], StatsBaseline>>>`. The poll loop is a `PROJECT.md` anti-pattern (magpie explicitly chose event-driven over poll); M2's gate includes replacing it with an event-driven equivalent.

Three consumers want stats, with different latency and shape requirements:

1. **The choker** (ADR-0012): per-peer 20 s EWMA rates, sampled every 10 s. Needs lock-free reads of a cumulative byte counter per peer.
2. **The shaper** (ADR-0013): per-peer consumed bytes since the last 100 ms refiller tick. Refiller computes demand as `consumed + denied`.
3. **The consumer** (lightorrent): cumulative uploaded / downloaded per torrent at ~1 Hz for UI, plus persistence across restarts so ratio enforcement survives a crash.

The first two read from the byte-path directly; the third wants an alert stream and a durable sink. Getting this wrong means either (a) hot-path atomics for things only the consumer cares about, (b) consumer reads that miss the event-driven contract, or (c) persistence that duplicates counters the in-core code already maintains.

## Decision

### One monotonic counter per peer, per direction

`PeerState` owns two `AtomicU64` counters:

```rust
struct PeerState {
    uploaded:   AtomicU64,   // bytes sent on the wire since peer attach
    downloaded: AtomicU64,   // bytes received on the wire since peer attach
    /* ... existing fields ... */
}
```

Updated on `fetch_add` once per send/recv on the peer task itself. These are the *only* byte counters on the byte path. They serve:

- **The choker** reads these directly every 10 s, computes delta since last sample, EWMA-updates. See ADR-0012.
- **The shaper's demand signal** (ADR-0013) computes `consumed = current_uploaded - prev_refiller_sample_uploaded` at each 100 ms refiller tick per direction. The refiller stores `prev_sample` per peer. **No separate shaper `consumed` atomic is maintained on the byte path** — a first-draft sketch had one, but reusing the stats counter saves one atomic per sent/received block. ADR-0013 amended accordingly.
- **The stats emitter** (below) reads them on the 1 Hz tick.

One atomic-add per block, three readers. No contention: one writer (the peer task), independent readers (different sampling tasks).

**Overflow**: `AtomicU64` at sustained gigabit = 125 MiB/s × ~292 years to wrap. Not a concern for any realistic session lifetime.

### Cumulative counters per torrent

Peer counters are lost when the peer disconnects. The torrent keeps them alive:

```rust
struct TorrentStats {
    uploaded_live_peers_snapshot:   AtomicU64,  // updated by the 1 Hz emitter
    downloaded_live_peers_snapshot: AtomicU64,
    uploaded_disconnected:   AtomicU64,  // sum of all disconnected peers' final counters
    downloaded_disconnected: AtomicU64,
}
```

On peer disconnect: torrent actor reads the peer's final `uploaded` / `downloaded`, `fetch_add`s them into `*_disconnected`, drops the peer. Nothing is lost.

**Two read paths, distinct cadences:**

- **The 1 Hz emitter** uses the snapshot: `total_uploaded = uploaded_live_peers_snapshot + uploaded_disconnected`. Snapshot is refreshed by the emitter tick itself; the alert emission is O(1) after the walk. This is the fast path for the high-frequency consumer.
- **The public API (`Session::torrent_stats`)** computes `total_uploaded = (Σ live_peer.uploaded) + uploaded_disconnected` **on demand**, walking live peers at call time. Never reads the snapshot. This matters for ratio enforcement and any other consumer that needs exact current values: reading the snapshot could return a value up to 1 second stale, which at gigabit equals ~125 MiB of overshoot before the next tick catches it. The ad-hoc walk at ~50 peers is a handful of `AtomicU64::load`s — invisible at the call frequency consumers actually use (once per torrent add, once per ratio check, not per byte).

Neither path is on the choker / shaper byte path — they read live-peer atomics directly at their own cadences (10 s and 100 ms respectively).

### 1 Hz `StatsUpdate` event

Each `TorrentActor` runs a 1-per-second tick that emits one alert:

```rust
pub struct StatsUpdate {
    pub info_hash: InfoHash,
    pub at: Instant,
    pub uploaded_total:     u64,   // cumulative since first add
    pub downloaded_total:   u64,
    pub uploaded_delta:     u64,   // since the previous StatsUpdate
    pub downloaded_delta:   u64,
    pub num_peers:          u32,
    pub num_unchoked_peers: u32,
}
```

- **Rate limit is 1 Hz per torrent**, not per session. 100 torrents → 100 events/s, well within the alert ring's steady-state budget (ADR-0002).
- **Deltas are precomputed** so consumers don't have to track previous samples to derive rates.
- **Consumer-facing only**. Explicitly not read by the choker, shaper, or any in-core subsystem. If an in-core subsystem needs a rate, it reads the atomics directly at its own cadence.

The 1 Hz tick doubles as the snapshot updater: it walks live peers, computes `total = Σ peer.uploaded + uploaded_disconnected`, writes to `uploaded_live_peers_snapshot`, then emits the alert.

### Persistence: `trait StatsSink`

```rust
pub trait StatsSink: Send + Sync {
    fn record(&self, info_hash: &InfoHash, uploaded: u64, downloaded: u64, at: SystemTime);
    fn flush(&self) -> Result<(), StatsError>;
}
```

- **`record`** is called once per `StatsUpdate` for each registered sink.
- **`flush`** is called on a `flush_interval` (default 30 s) and on graceful shutdown. **Shutdown flush has a timeout** (default `flush_timeout = 5 s`, configurable). If a sink's `flush` doesn't return within the timeout, the session logs a warning alert and proceeds with shutdown anyway. A hung sink (network-backed, redb compaction, disk stall) must not block session teardown indefinitely — the 30 s of at-risk data is already an accepted loss envelope, and losing it to a hung sink is no worse than losing it to a crash. Blocking shutdown on a hung sink would be strictly worse than both.
- **Object-safe**, so session holds `Vec<Arc<dyn StatsSink>>`. Multiple sinks permitted (e.g. a file sink + a lightorrent-redb sink concurrently).

**Default implementation**: `FileStatsSink`. Writes to a `.stats` sidecar file next to the `.bitv` resume bitfield. Bencode format (magpie uses bencode for every on-disk format; consistency):

```
d
  4:info d20:<info_hash> e
  8:uploaded i<N>e
  10:downloaded i<N>e
  10:updated_at i<unix_ts>e
e
```

One file per torrent. Writes are batched via `flush_interval` (30 s default) + on-shutdown. Between flushes, records are held in a `BTreeMap<InfoHash, PendingRecord>` internal to the sink; flush rewrites the affected files atomically (tmp + rename).

**Unclean-shutdown window**: a crash loses up to `flush_interval` of cumulative progress (default 30 s). Operators who want zero loss raise the interval to 1 s or implement their own `StatsSink` with a WAL. Lightorrent's redb-backed sink gets WAL semantics for free from redb.

### Subscription wiring

The session spawns one `StatsSubscriber` task per `StatsSink`:

```
alert ring ──(filter: Alert::Stats)──> StatsSubscriber ──> sink.record()
```

Each subscriber drains the ring with the `STATS` category mask and calls `record`. A separate timer task calls `flush` every `flush_interval`. Decoupled so a slow sink cannot backpressure the emitter — it drops events (with a `dropped` alert) rather than stalling.

### No internal session stats API beyond this

Consumers that want current totals either (a) subscribe to `StatsUpdate`, or (b) ask the session's public API, which reads `total_uploaded = snapshot + disconnected`. No `Session::poll_stats` helper — if lightorrent wants the current value outside the 1 Hz cadence, the API is "ask once" not "poll in a loop." The 1 Hz event drives the common case; the ad-hoc read handles the "just added a torrent, what's the baseline?" case.

## Consequences

Positive:

- **One atomic per block** for all three consumers (choker / shaper / stats). ADR-0013's original sketch had two; the amendment collapses them.
- **Event-driven, no poll**. Replaces lightorrent's current poll loop per M2 gate.
- **Persistence is pluggable**. Lightorrent keeps its redb-backed store (WAL, transactional) without magpie having a redb dependency. Default file sink serves the no-consumer case.
- **Cumulative counters are correct across peer churn**. Disconnection moves the peer's contribution into `*_disconnected`; no byte is lost.
- **Unclean-shutdown loss is bounded and tunable**. Default 30 s is the cost of cheap batched writes; tighter is one config change.
- **Multiple sinks supported**. A diagnostics sink + a production redb sink can run concurrently without coordination.

Negative:

- **Disconnected-contribution aggregation touches a torrent-level atomic on every peer disconnect**. Rare event; contention is zero in practice.
- **The 1 Hz snapshot walks all live peers once per second per torrent**. At 100 torrents × 50 peers = 5 000 atomic reads/s — invisible.
- **Sink backpressure policy is drop-and-alert**, not blocking. A sink that can't keep up loses events. Operators see `Alert::Dropped` when this happens. Blocking the emitter would make stats a liveness liability.
- **No intra-second resolution of rate**. If an operator wants 10 Hz stats for a monitoring dashboard, they have to read atomics directly via the public API. The event stream is 1 Hz by contract.
- **`.stats` sidecar per torrent** means many small files in the default sink. Fine at M2 scale; a consumer with 10k torrents should swap to a single-file sink.

## Alternatives considered

- **Stats inside the library as a first-class owned database.** Rejected per `research/SUMMARY.md`: consumer state is consumer-owned. Every reference implementation that bundled stats persistence regretted it (non-portable across consumer storage choices). Trait + default + consumer override is the correct boundary.
- **Torrent-level atomics updated on every byte by the peer task** (option B in the notes). Rejected: adds cross-peer contention on a shared atomic per block sent. Aggregating at 1 Hz is free and the torrent-level counter isn't needed on the byte path.
- **Separate shaper `consumed` atomic alongside stats counters**. First-draft ADR-0013 shape. Rejected in favour of the refiller computing `consumed = current - prev_sample` from the existing stats counter — saves one atomic per byte-path op.
- **Emit `StatsUpdate` on every block**. Rejected: at gigabit with 16 KiB blocks = 8 000 events/s per torrent. Overwhelms the alert ring and gives consumers higher resolution than any UI needs. 1 Hz is the consumer cadence.
- **Push-mode sink** where stats emit directly into a user callback. Rejected: synchronous callback pattern is anacrolix's pain point per `research/002`. Alert ring + subscriber task is the decoupled shape ADR-0002 already chose.
- **JSON / TOML stats file format.** Rejected for consistency: magpie's other on-disk formats are bencode. One format to parse, one fuzz target category.
