# 0015 — UDP socket demultiplexer

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: research/003 (rasterbar UDP socket manager), BEP 15 (UDP tracker protocol), BEP 5 (DHT), BEP 29 (uTP), experience of projects that *didn't* share a UDP port across subsystems

## Context

M2 introduces UDP for BEP 15 (UDP tracker). M3 and M4 will add two more UDP-speaking subsystems: DHT (BEP 5) and uTP (BEP 29). BitTorrent-wide convention is that these three share a single listen port so that a single NAT port-forward or UPnP mapping covers all of them — ports separately mapped for tracker, DHT, and uTP are an operator footgun that most implementations have experienced and regretted. Rasterbar's `udp_socket_manager` is the reference design; we're following it, translated to tokio.

The M2-only decision is narrower: we're committing to the *shape* now even though only UDP-tracker actually subscribes. The cost of not committing is a painful refactor when M3 DHT and M4 uTP each need to bolt onto an already-owned socket.

### Constraints

1. **One `tokio::net::UdpSocket` per listen port.** Sharing the port is the whole point. A per-subsystem socket is the footgun we're avoiding.
2. **Dispatch by packet content, not by port.** DHT, tracker, and uTP all arrive on the same port; the demultiplexer must look at the first bytes to decide whose inbox the packet belongs in.
3. **Subscribers are in-process tasks.** Each subsystem runs as a tokio task reading from an `mpsc::Receiver<(Vec<u8>, SocketAddr)>` (or equivalent). The demux's job is `recv_from` in a loop and route.
4. **Hot path is the byte path for DHT (M3)** and eventually uTP (M4), so the demux dispatch must be cheap — first-byte check, match, channel send.
5. **Unknown packets are dropped silently** with a counter. Logging per unknown packet is a DoS vector (attacker floods malformed packets to fill logs).

### Packet identification

Each protocol's wire format has a recognisable leading byte or byte pattern:

- **BEP 15 UDP tracker responses**: start with a 4-byte `action` field. Values are `0` (connect), `1` (announce), `2` (scrape), `3` (error). But tracker responses *don't identify themselves uniquely at byte 0* — they start with a u32 action followed by a u32 transaction_id. The demux can't reliably identify an inbound tracker response by first-byte alone — what it *can* do is maintain a registry of outstanding tracker transactions (transaction_id → tracker client). See tracker routing below.
- **BEP 5 DHT**: KRPC over bencode. A DHT packet starts with `d` (the bencode dictionary opener) since the top-level of every KRPC message is a dict. Unambiguous.
- **BEP 29 uTP**: starts with a specific byte combining version (`0x01` low nibble for v1) and packet type (high nibble, `0x0`–`0x4`). First byte in v1 is always one of `{0x01, 0x11, 0x21, 0x31, 0x41}`. Unambiguous.
- **Anything else**: dropped.

## Decision

### Shape

One `UdpDemux` actor owned by the `Session`, spawned at session start, bound to the configured listen port on both IPv4 and IPv6 where available. It runs a `recv_from` loop and dispatches to per-subsystem channels:

```rust
pub struct UdpDemux {
    socket: Arc<UdpSocket>,
    dht_tx: Option<mpsc::Sender<UdpPacket>>,
    utp_tx: Option<mpsc::Sender<UdpPacket>>,
    tracker_routing: Arc<TrackerTxnRegistry>,  // see below
    unmatched_drops: AtomicU64,
}

pub struct UdpPacket {
    pub data: Vec<u8>,
    pub from: SocketAddr,
    pub received_at: Instant,
}
```

In M2 only `tracker_routing` is populated. `dht_tx` and `utp_tx` are `None` and their dispatch branches drop silently + increment `unmatched_drops`.

### Dispatch logic

```
on recv_from → (data, from):
    if data.is_empty() { drop; continue }
    match data[0] {
        b'd' => if let Some(tx) = dht_tx { tx.try_send(...) } else { drop }
        0x01 | 0x11 | 0x21 | 0x31 | 0x41 => if let Some(tx) = utp_tx { ... } else { drop }
        _ => tracker_routing.try_route(data, from)  // transaction-id-based
    }
```

Pattern match on a single byte. Branch-predictable, one cache line. The M3/M4 landings register their senders with the demux (`UdpDemux::register_dht`, `register_utp`) and the `None → Some` transition makes those branches hot instead of dropping.

**Fallthrough safety**: BEP 15 tracker response action values 0–3 (connect, announce, scrape, error) encode into the first 4 bytes as a big-endian `u32`, so `data[0] == 0x00` for every valid tracker response. `0x00` collides with neither `b'd'` (`0x64`) nor the uTP byte set (`0x01..=0x41`). A tracker response therefore always falls through to the transaction-id dispatch branch, never misrouted. Corollary: a packet whose `data[0]` is any of the DHT/uTP discriminators is *not* a tracker response, so the early branches don't steal tracker traffic.

### Tracker response routing

UDP tracker responses can't be identified by a byte pattern — they look like arbitrary bytes from the wire. But the BEP 15 protocol guarantees every response has a `transaction_id` at bytes `[4..8]` matching the request we sent. The demux keeps a registry:

```rust
struct TrackerTxnRegistry {
    // transaction_id -> response channel
    pending: DashMap<u32, oneshot::Sender<UdpPacket>>,
}
```

- The UDP tracker client allocates a random `u32 transaction_id`, registers a `oneshot::Sender`, sends the request, awaits the receiver.
- On response arrival, the demux decodes `bytes[4..8]` as the transaction id, looks up the sender, and delivers.
- No match → unknown transaction (stale response, attacker-spoofed packet with unknown id, etc.) → drop + counter.

Registry entries have a TTL (60 s default) swept by a background task on a **10 s interval** (so worst-case lifetime is 70 s: registered 1 s after a sweep, TTL-expires, caught by the next sweep). 10 s balances freshness against sweep cost — the cap is 10 000 entries, so each sweep is a linear scan of at most 10 000 `DashMap` entries with a timestamp comparison, trivially cheap at 10 s cadence. Lost responses get cleared rather than accumulating. The tracker client times out the `oneshot::Recv` on its own schedule (usually shorter than the registry TTL) so a lost response surfaces to the caller as a timeout, not an orphan.

### Subscriber registration

```rust
impl UdpDemux {
    pub fn register_dht(&mut self, tx: mpsc::Sender<UdpPacket>);
    pub fn register_utp(&mut self, tx: mpsc::Sender<UdpPacket>);
    pub fn tracker_txn_registry(&self) -> Arc<TrackerTxnRegistry>;
}
```

`register_*` methods return `Result` — registering twice is an error, protecting against accidental double-subscription when a subsystem is restarted. The tracker registry is `Arc`-shared because the UDP tracker client pool creates the transaction ids (one per outstanding request) and needs read/write access.

### Bounded inboxes; drop policy

Each subscriber channel is bounded (default 1024 packets, configurable). `try_send` is used so the demux never awaits on the channel; if a subsystem's inbox is full, the packet drops and `unmatched_drops` increments. This is correct behaviour:

- A DHT inbox full means the DHT task is overwhelmed and extra packets are surplus to its processing rate.
- A uTP inbox full means we're losing uTP segments, which uTP's own retransmission handles.

Blocking the demux on a slow subsystem would pause *all* protocols on the shared socket — including tracker responses a DHT slowness has nothing to do with.

### Batch receive (`recvmmsg`) hook

Placeholder only for M2. The `recv_from` loop calls `socket.recv_from` one packet at a time. The subscriber interface accepts `UdpPacket` singletons, but the actual inbox type (`mpsc::Sender<UdpPacket>`) can be replaced with a small-batch sender without API change. When/if we add a Linux `recvmmsg` batch path, the demux reads up to N packets in one syscall and bulk-dispatches. DHT is the first subsystem that will benefit (high packet rates from find_node sweeps); uTP next. Not shipping in M2; flagged so the design doesn't preclude it.

## Consequences

Positive:

- **One NAT mapping / port-forward suffices for tracker + DHT + uTP.** The operator ergonomics that motivated this ADR.
- **First-byte dispatch is essentially free.** Match on one byte, route to a channel, continue. Well below 1 µs per packet on any modern hardware.
- **M3 and M4 subscribe without rewiring anything.** `register_dht`/`register_utp` flip a `None → Some` and the dispatch branch becomes live. No demux refactor, no tracker impact.
- **Unknown packets drop silently with a counter.** Attackers who spam malformed packets cost us memory (the packet buffer, freed immediately) and the counter increment — no log flood, no unbounded state.
- **Tracker transaction routing is the only non-trivial bit.** Handled with a `DashMap` registry and TTL sweeper; both well-understood patterns.
- **Subscriber backpressure never blocks the demux.** Slow subsystems lose their own packets, not everyone's.

Negative:

- **`recv_from` in a loop is one syscall per packet.** Fine for M2's tracker rates (maybe 1 announce every 30 min per torrent); not great for M3 DHT rates (potentially thousands of packets/s during a swarm bootstrap). The `recvmmsg` batch hook is the known mitigation; implementing it is an M3 concern, not M2.
- **`UdpPacket::data: Vec<u8>` allocates per packet.** Invisible at M2 tracker rates; visible at M3 DHT rates as one `Vec` alloc + copy per packet. Obvious mitigation: a `BytesMut` slab pool, recycled after the subscriber drops its reference. Same shape as `recvmmsg` — flagged so the M3 DHT landing can optimise without changing the subscriber channel surface.
- **`DashMap` for the transaction registry** adds a dep. DashMap is already transitive via tokio in some feature combinations; if it isn't, `Mutex<HashMap>` is the fallback with modest contention at low transaction rates (M2 scale is fine either way).
- **Attacker can fill the transaction registry** with spoofed-source requests to non-existent transaction ids. Bounded by the 60 s TTL and a hard cap (default 10 000 outstanding) — beyond that, new outbound transactions fail fast and the caller retries later. Cap is low enough to be immaterial against a botnet, high enough that normal tracker loads never hit it.
- **IPv6 socket separate from IPv4.** Two demuxes if both are configured, each with its own subscribers. Adds a multiplier to registration calls but no interesting logic. Can be collapsed if we adopt dual-stack `IN6ADDR_ANY` sockets later; platform-specific.

Neutral:

- The demux lives in `magpie-bt-core`, not in a separate crate. Per ADR-0001, DHT and uTP are subcrates; they register with the demux through the core API. No architectural coupling.

## Alternatives considered

- **One UDP socket per subsystem** (tracker / DHT / uTP each own a socket). Rejected: requires separate port mappings, is the operator footgun we're avoiding, and is what projects who later adopted shared sockets said they regretted.
- **Broadcast all packets to all subsystems, let each filter themselves.** Rejected: each subsystem would run the first-byte check, multiplying the work. Single-writer dispatch is strictly better.
- **Dispatch on source-address (which tracker is that? which peer?) rather than content.** Rejected: tracker and DHT use ephemeral UDP ports on the remote; we cannot pre-associate a source address to a subsystem. Content-based dispatch is the only reliable signal.
- **Log every unmatched packet.** Rejected: log flood DoS vector. Counter + periodic summary alert is the observability equivalent.
- **Tracker transaction registry as `Mutex<HashMap>` from day one.** Fallback; `DashMap` is preferred for the concurrent-read pattern. Keep `DashMap` behind a feature flag if the transitive dep situation changes; otherwise default.
- **Synchronous demux (blocking syscalls on a dedicated thread).** Rejected: `tokio::net::UdpSocket` is the idiomatic choice and integrates with the rest of the runtime (ADR-0003). No performance win from going synchronous at M2's UDP rates.
- **Bounded inboxes block the demux on full** instead of drop. Rejected explicitly above: one slow subsystem must not starve the others.
