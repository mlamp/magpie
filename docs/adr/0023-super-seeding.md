# 0023 — Super-seeding (BEP 16)

- **Status**: proposed
- **Date**: 2026-04-20
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/src/protocol/initial_seed.{h,cc}`, ADR-0012 (choker), ADR-0019 (completion transition)

## Context

When a new seed enters a swarm with the only complete copy of a torrent,
naïvely serving every peer that connects is pathologically inefficient:
upload bandwidth is single-homed, and duplicated deliveries to peers
that already have a piece waste that budget. BEP 16 specifies
"super-seeding" (rakshasa calls it *initial seeding*) as the
counter-strategy:

1. Pretend we don't have any pieces — advertise `HaveNone` / empty
   bitfield at handshake.
2. Reveal one piece per peer via `Have`. Track who was assigned what.
3. Only serve requests for pieces we have revealed to that peer.
4. When a peer uploads the revealed piece to other peers (detected via
   those peers advertising it back to us), assign a new piece to the
   source peer.
5. After each piece has been revealed ~1.5× its "fanout" times, fall
   back to normal seed mode.

Today magpie's `SeedChoker` (ADR-0012) ranks by upload rate and unchokes
the fastest consumers. That's the right strategy for a steady-state seed
but not for a fresh seed joining a thin swarm — all our bandwidth goes
to whichever 4 peers happen to ask first.

This ADR designs the super-seed mode. **Implementation is separate** —
this document lands as `status: proposed` ahead of execution so the
design is reviewed before code moves.

## Decision

Introduce a new `SuperSeedChoker` + per-peer `RevealState` tracker
orthogonal to the existing leech/seed choker split. Super-seeding is
opt-in via `AddTorrentRequest::super_seed: bool` (default `false`).

### State

```rust
pub struct SuperSeedState {
    /// Per-peer revelation tracking. `offered_piece` is the piece we
    /// most recently revealed; `offered_at` bounds how long we wait
    /// before reassigning if the peer stalls.
    per_peer: HashMap<PeerSlot, PeerReveal>,
    /// Per-piece fanout: how many times have we revealed piece `i` to
    /// someone. Used to pick next revelation (lowest-fanout piece wins)
    /// and to decide when to exit super-seed mode.
    fanout: Vec<u32>,
    /// Cumulative pieces uploaded full (for the fallback trigger).
    pieces_completed_upload: u64,
    /// Config: max fanout before we fall back to normal seed mode.
    max_fanout: u32,
}

struct PeerReveal {
    offered_piece: Option<u32>,
    offered_at: Instant,
    /// Has the peer advertised the offered piece as "have" back to us?
    /// (Via another peer who got it first, or from us post-upload.)
    delivered: bool,
}
```

### Control flow hooks

- **Handshake** (`session/peer.rs`): when `super_seed` is true, send
  `HaveNone` (fast-ext peer) or an empty bitfield (legacy peer) instead
  of our full bitfield.
- **Peer registered** (`session/torrent.rs::register_peer`): initialise
  a `PeerReveal` with no offered piece. Pick the first revelation via
  `SuperSeedState::next_piece_to_reveal(peer)` which returns the
  lowest-fanout piece this peer doesn't already claim to have. Emit
  a one-off `Have` for that piece.
- **Piece requested** (`session/peer_upload.rs`): before accepting a
  request, check that the piece is the one we revealed to this peer.
  If not: reject (with `RejectRequest` on fast-ext peers, or a silent
  choke for legacy peers).
- **Peer bitfield/have update**: if the peer now advertises the piece
  we revealed, mark `delivered = true` and pick the next piece to
  reveal for them (`fanout[old_piece] += 1`).
- **Rotation timer** (1 min, opt-in knob like ADR-0012's timers):
  on any peer whose offered piece has been held >2 minutes without
  delivery, reassign to a different lowest-fanout piece.
- **Fallback**: when `fanout.iter().all(|f| f >= max_fanout)` (every
  piece has been revealed at least `max_fanout` times, typical 2),
  flip the torrent into normal seed mode atomically and stop consulting
  `SuperSeedState`. Fires an `Alert::SuperSeedComplete`.

### Choker integration

`SuperSeedChoker` is not a rate-based choker — it unchokes exactly
those peers who have an active revelation and haven't yet delivered it.
Regular slot count + optimistic slot count are reinterpreted as
"maximum concurrent revelations" — at most `slots` peers are mid-
delivery at any one time. Bandwidth is therefore spent spreading
unique pieces rather than serving the fastest.

### Invariants

1. A peer is only unchoked if they have an active revelation.
2. A request is only honoured if it targets the revealed piece.
3. `fanout[i]` monotonically increases.
4. Exit-to-normal-seed is one-way: once flipped, we never return to
   super-seed mode for this session.

### Interaction with existing systems

- **Completion transition (ADR-0019)**: unaffected — super-seeding runs
  in the already-completed state. A torrent loaded complete-from-resume
  *can* opt in to super-seed at `add_torrent` time; the transition
  alert is skipped per ADR-0019 anyway.
- **Choker (ADR-0012)**: `SuperSeedChoker` replaces `SeedChoker` for
  super-seeding torrents. On fallback, the actor swaps the chokers
  (same enum-switch the leech→seed transition uses).
- **Stats (ADR-0014)**: `uploaded` accrues normally. New counter
  `super_seed_revelations: AtomicU64` for observability.
- **Private torrents (BEP 27)**: orthogonal. Super-seeding a private
  torrent is valid; PEX/LSD/DHT suppression still applies.

## Consequences

Positive:

- Fresh-seed bandwidth goes to spreading pieces, not duplicating
  deliveries to the fastest 4.
- Matches rakshasa's `initial_seed.cc` shape at the behavioural level
  without copying code.
- Opt-in: existing consumers get no behaviour change.

Negative:

- Non-trivial scope: touches the peer handshake (bitfield emission),
  the request-acceptance path, the choker, and adds a new per-peer
  state machine. Estimated **2–3 weeks of focused work**, not the
  "~1 week" earlier planning assumed.
- A misbehaving peer can stall progress by never actually uploading
  the revealed piece. Mitigation: reassign after 2 minutes of
  no-delivery (see rotation timer above) — rakshasa uses 5 minutes;
  we pick 2 minutes as a more aggressive default for today's faster
  networks.
- `fanout` vec is `Vec<u32>` of length `piece_count`. For a 100K-piece
  soak torrent that's 400 KB — acceptable.

Neutral:

- This ADR is **proposed**, not accepted. Implementation blocks on a
  follow-up commit that lands the state + hooks + tests. Until then
  `AddTorrentRequest::super_seed` does not exist.

## Alternatives considered

- **Naïve "throttled advertisement"**: rate-limit `Have` messages to
  simulate super-seed behaviour without the per-peer tracking.
  Rejected: BEP 16 compliance requires the per-peer revelation state
  machine; a rate-limit alone would accidentally duplicate deliveries
  to peers who asked first.
- **Implement on top of `SeedChoker` via a new `Unchoker::want_piece`
  hook** (the "need_set" line from the earlier M2 milestone text).
  Rejected: super-seeding has different *request-acceptance* semantics
  on top of different unchoke semantics; bolting both onto one
  `Unchoker` confuses the two concerns. A dedicated
  `SuperSeedChoker` + `SuperSeedState` is clearer.
- **Implement in a follow-up M7-era milestone**: earlier planning
  assumed this. Rejected now because it's a load-bearing feature for
  any consumer seeding fresh content (game releases, video creators) —
  waiting to M7 means magpie can't be used for those scenarios.
  Promote to Tier 2 of the parity push (after DHT).

## Execution checklist (deferred — this ADR is design-only)

- [ ] `session/super_seed.rs` new module with `SuperSeedState` + tests.
- [ ] `SuperSeedChoker` impl of `Unchoker`.
- [ ] Bitfield/HaveNone emission gate in `session/peer.rs` handshake.
- [ ] Request-piece-match gate in `session/peer_upload.rs`.
- [ ] `register_peer` integration to seed the initial revelation.
- [ ] Rotation timer + reassignment on stall.
- [ ] Fallback-to-normal-seed detection + choker swap.
- [ ] `AddTorrentRequest::super_seed: bool`.
- [ ] Integration test: 1 super-seed + 3 leechers, verify fanout after
      complete swarm download lands in expected range (each piece seen
      ≥`max_fanout` times).
- [ ] Interop scenario against qBittorrent in `ci/interop/`.
