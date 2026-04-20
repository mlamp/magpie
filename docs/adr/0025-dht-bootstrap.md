# 0025 — DHT bootstrap strategy (BEP 5)

- **Status**: proposed
- **Date**: 2026-04-20
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/src/dht/dht_router.cc:602-663`, BEP 5 §"Bootstrap", ADR-0024 (routing table)

## Context

A cold DHT node with no routing table needs to discover peers before
it can serve lookups. BEP 5 is silent on bootstrap sources; real-world
implementations use a small set of well-known hostnames plus any
previously-cached contacts. This ADR locks the bootstrap sources,
the zero-state recovery flow, retry cadence, and the exit criterion
that transitions a node from "bootstrapping" to "operational".

Reference: rakshasa ships this in `dht_router.cc:602-663` (`bootstrap()`
driving an explicit contact list of up to 64 addresses, pinging up to
8 at a time every 60 s, exit after 32 nodes collected).

## Decision

### Bootstrap sources (in priority order)

1. **Persistent cache** — a serialised contact list from the previous
   run, stored as a bencode sidecar under the consumer-supplied
   `cache_dir`. Schema mirrors [ADR-0022](0022-resume-state.md)'s
   `FileResumeSink` — atomic write-to-tmp + rename, size-capped at
   1 MiB (well over the 64-contact practical limit).
2. **DNS bootstrap hostnames** — magpie ships a default list:
   - `dht.transmissionbt.com:6881`
   - `router.bittorrent.com:6881`
   - `router.utorrent.com:6881`
   - `dht.libtorrent.org:25401` (rasterbar-hosted)
   These resolve via `tokio::net::lookup_host`; A/AAAA records all
   considered. Default list overridable via
   `DhtConfig::bootstrap_hostnames`.
3. **Consumer-supplied peers** — `Dht::add_bootstrap_contact(addr)`,
   e.g. the `x.pe` parameters from a magnet link, or magnet tracker
   URLs whose IPs the consumer passed to the DHT.

The cache is always consulted first (cold-start latency win when
re-opening magpie on a known host). DNS + consumer-supplied sources
are merged after resolution.

### Zero-state flow

From an empty routing table:

1. **Fill contact list** from the three sources above. Cap the
   aggregate at **64 addresses** (matches rakshasa — enough to tolerate
   a handful of stale cache entries without flooding).
2. **Every 60 s while bootstrapping**:
   - Pick up to **8 newest contacts** (by source order — cache first,
     then DNS, then consumer) that haven't been pinged this round.
   - Send `ping` to each. On reply, insert into the routing table
     (triggering splits per ADR-0024).
   - Send `find_node(local_id)` to each successful contact to widen
     the pool.
   - Also send `find_node(random_id)` to one random questionable node
     per round to cover the ID space beyond our immediate neighbourhood.
3. **Exit criterion**: transition to operational when BOTH hold:
   - Routing table has ≥ **32 good nodes** total (matches rakshasa's
     `num_bootstrap_complete = 32`).
   - At least one `find_node` round has returned a non-empty
     `r::nodes` response (proves the DHT-at-large is reachable, not
     just the bootstrap contacts).
4. **Failure case**: after 10 minutes with fewer than 4 good nodes,
   fire `Alert::DhtBootstrapStalled { contacts_tried, good_nodes }`
   and keep retrying at reduced cadence (one ping round every 5 min
   instead of every 60 s) until shutdown.

### Operational refresh

Post-bootstrap, the routing-table refresh timer (ADR-0024) takes over.
No separate "bootstrap" code path stays live — the state machine
transitions the DHT from `Bootstrapping` to `Operational` on exit
criterion, and back to `Bootstrapping` only if every good node goes
`Bad` (catastrophic network event).

### Persistence

On graceful shutdown, serialise the **top 64 good nodes by last_seen
recency** as the persistent contact cache. Atomic write via the same
write-to-tmp + rename pattern as `FileResumeSink`. Corrupted or
oversize cache files are ignored on next startup (fail open — try
DNS).

### BEP 42 note

Local node ID generation + BEP 42 IP salt live in ADR-0026. This
ADR assumes the local ID is already set at bootstrap start.

## Consequences

Positive:

- Three-source bootstrap survives any one source failing: DNS down →
  cache + consumer-supplied still work; empty cache + no consumer
  contacts → DNS alone still works.
- Explicit 32-node + at-least-one-hop exit criterion rules out a
  degenerate "bootstrapped only via the 4 hardcoded hostnames" state
  that would pass a simpler `≥ N nodes` check.
- Persistent cache means warm restart latency is near-zero — routing
  table is populated before the first `announce_peer`.

Negative:

- Hardcoded bootstrap hostnames are a trust anchor. If all four go
  down simultaneously, a cold node with no cache can't bootstrap.
  Mitigated by the consumer-supplied bootstrap hook (`x.pe` parameters
  from a magnet link, LAN peers from LSD, peers learnt via tracker —
  any of these can seed the DHT).
- Stalled-bootstrap alert fires after 10 min, which is long if the
  consumer expects a fast UX. Acceptable: M4 is library-surface, the
  consumer decides UI.

Neutral:

- No DHT-over-IPv6 in v1. Most deployed DHTs are v4-only; adding v6
  bootstrap addresses is a config-list update when we have demand.

## Alternatives considered

- **Ship a much larger DNS-hostname list** (20+). Rejected: larger
  list adds resolution latency at startup without meaningful
  reachability improvement; 4 well-known is rakshasa precedent and
  sufficient.
- **Ping every cache entry on startup** (not just the newest 8).
  Rejected: floods the network with unnecessary pings for a cache
  that's likely 90% fresh. The 8-at-a-time cadence matches rakshasa
  and is gentle on the network.
- **Exit bootstrap on ≥ 8 good nodes** (one bucket's worth).
  Rejected: 8 is too thin — the first bucket might be full of
  bootstrap-relay nodes with no actual swarm coverage. 32 forces
  multi-hop discovery.
- **Store the cache in the engine-global stats/resume directory**.
  Rejected: cache is DHT-specific; grouping with torrent resume state
  is semantically confusing. Separate `cache_dir` parameter.
