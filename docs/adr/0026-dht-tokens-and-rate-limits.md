# 0026 — DHT tokens, BEP 42 node ID, and rate limiting

- **Status**: proposed
- **Date**: 2026-04-20
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/src/dht/dht_router.cc:436-514`, BEP 5 §"announce_peer", BEP 42, ADR-0024 (routing table), ADR-0025 (bootstrap)

## Context

DHT security has three load-bearing pieces:

1. **`announce_peer` tokens** — the DHT must bind a `get_peers` reply
   to the specific peer that received it, so later `announce_peer`
   calls can't be forged from arbitrary source IPs.
2. **BEP 42 node ID salting** — a node's ID must be derived from its
   IP, so Sybil attacks (one attacker spinning up thousands of node
   IDs to flood a swarm) are computationally expensive.
3. **Rate limiting** — a DHT exposes a UDP service to the internet;
   without rate limits, a flood of queries trivially exhausts CPU or
   memory.

Rakshasa ships #1 (stateless HMAC of IP + rotating secret), skips #2,
and has no explicit rate limiting. Magpie mirrors #1, adds #2
(general-purpose library must defend against Sybil abuse out of the
box), and adds #3 (public library on the internet with a UDP listener
is a DoS magnet otherwise).

## Decision

### `announce_peer` tokens — stateless HMAC

Mirror rakshasa's design:

```rust
fn make_token(peer_ip: IpAddr, secret: [u8; 20]) -> [u8; 8] {
    let mut hasher = Sha1::new();
    hasher.update(secret);
    hasher.update(ip_bytes(peer_ip));
    let full = hasher.finalize();
    full[..8].try_into().unwrap()
}
```

Two secrets rotated every **15 minutes**:

```rust
struct TokenSecrets {
    current: [u8; 20],
    previous: [u8; 20],
    rotated_at: Instant,
}
```

Rotation: every 15 min, `previous = current; current = random()`.
Validation: `token == make_token(ip, current) || token == make_token(ip, previous)`.
This gives a **30-minute validity window** — longer than realistic
`get_peers` → `announce_peer` latency, short enough that leaked
tokens expire quickly.

**Why stateless**: no per-peer token-issuance tracking. A restart
loses the secrets (rotates on init) and all in-flight tokens become
invalid — acceptable, the peer just has to re-`get_peers`.

### BEP 42 node ID salting

Local node ID generation:

```rust
fn generate_node_id(public_ip: IpAddr, rand: u32) -> [u8; 20] {
    // BEP 42: id = crc32c(ip_masked ++ rand_lsb) << 21 | random_tail
    let ip_bytes = match public_ip {
        IpAddr::V4(v4) => v4.octets().iter().zip([0x03, 0x0f, 0x3f, 0xff])
            .map(|(a, m)| a & m).collect::<Vec<_>>(),
        IpAddr::V6(v6) => /* first 8 bytes masked */,
    };
    let mut crc_input = ip_bytes;
    crc_input.push((rand & 0x7) as u8);
    let crc = crc32c(&crc_input);
    // Top 21 bits of id = top 21 bits of crc
    let mut id = [0u8; 20];
    id[0] = ((crc >> 24) & 0xff) as u8;
    id[1] = ((crc >> 16) & 0xff) as u8;
    id[2] = (((crc >> 8) & 0xf8) | (rand & 0x7)) as u8;
    // Bottom 152 bits random
    rng.fill_bytes(&mut id[3..]);
    id
}
```

Public IP discovery: **lazy** — use `IpAddr::UNSPECIFIED` (random ID
without BEP 42 salt) at startup, and re-derive the salted ID after
the first peer's `yourip` (BEP 10) or tracker announce echo reveals
our public IP. Consumers that know their public IP up-front can pass
it via `DhtConfig::public_ip` to skip the two-phase ID derivation.

**Validation on inbound**: optionally reject nodes whose ID doesn't
match BEP 42 for their source IP. Not enforced by default (many
legitimate legacy clients don't BEP-42-salt); exposed as
`DhtConfig::strict_bep42_inbound: bool` off by default.

### Rate limiting

Three tiers of limits, all using token buckets:

1. **Per-source-IP inbound**: 20 queries/sec sustained, burst 60.
   Prevents a single host from saturating our DHT loop.
2. **Global inbound**: 500 queries/sec sustained, burst 1500.
   Matches a 1-Gbit-NIC's capability at ~1000-byte packets but caps
   CPU use to a reasonable fraction.
3. **Per-remote-node outbound**: 10 queries/sec per remote. Prevents
   runaway recursion in our own search code from DoSing a single
   honest node.

Over-limit inbound: silently drop the datagram, increment
`queries_dropped_rate_limited` counter. No reply sent — a rate
limiter that sends "you're being rate-limited" replies is itself a
reflection amplifier.

Over-limit outbound: defer the query via a per-remote delay queue
(bounded at 100 per remote — overflow → drop the query, returning
`TrackerError::RateLimited` to the caller).

### Constants

All exported from `magpie-bt-dht`:

```rust
pub const TOKEN_ROTATION_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const TOKEN_LENGTH: usize = 8;
pub const DEFAULT_INBOUND_PER_IP_QPS: u32 = 20;
pub const DEFAULT_INBOUND_GLOBAL_QPS: u32 = 500;
pub const DEFAULT_OUTBOUND_PER_NODE_QPS: u32 = 10;
```

All three QPS caps overridable via `DhtConfig`.

## Consequences

Positive:

- Stateless tokens: no per-peer bookkeeping, no scaling concerns, no
  cache-eviction bugs. Matches the design rtorrent has run in
  production for two decades.
- BEP 42: Sybil attackers can't mint IDs freely — they must either
  control enough IPs to form a Kademlia-adjacent cluster (expensive)
  or fail validation (and be rejected if strict mode enabled).
- Rate limits use token buckets already present in the magpie
  bandwidth shaper (ADR-0013), so the implementation reuses that
  infrastructure.
- Silent drop for rate-limited inbound avoids the DHT becoming a
  reflection amp.

Negative:

- Token secrets rotate on restart, so any in-flight `get_peers`
  tokens become invalid. Acceptable — the peer retries and gets a
  fresh token.
- BEP 42 two-phase derivation is a corner case: an ID generated
  pre-public-IP-discovery is un-salted, and once we re-derive, we
  break ID-continuity for remote nodes that cached us. Mitigation:
  announce a new node id via routing-table refresh (remote gets a
  `find_node` from our new ID and updates their bucket).
- Default rate limits are a tuning trade-off. Too strict → slow
  lookups under load. Too loose → DoS surface. Constants chosen to
  match order-of-magnitude what libtorrent-rasterbar documents; the
  first real deployment will refine them.

Neutral:

- No IPv6-specific rate-limit tuning. IPv6 flood resistance is a
  follow-up when we have a v6-adoption signal.

## Alternatives considered

- **Per-peer stateful token tracking** (remember we issued token T to
  peer P, invalidate on receipt). Rejected: O(peers × torrents)
  state for no security win over the stateless HMAC design, plus
  introduces a cache-eviction policy we'd need to get right.
- **SHA-256-based tokens** instead of SHA-1. Rejected: 8 bytes of
  SHA-1 output is ~2^63 brute-force, which is more than the 30-minute
  validity window can exploit. Upgrading to SHA-256 is a strictly
  bigger response (unchanged security in practice).
- **Skip BEP 42**. Rejected: rakshasa's choice not to implement it is
  a product of its 2001-era threat model. Magpie as a 2026 public
  library must resist Sybil.
- **Leaky-bucket rate limiting** instead of token bucket. Rejected:
  token bucket matches ADR-0013's existing shaper, letting us reuse
  infrastructure.
