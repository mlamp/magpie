# 0009 — Peer connection state machine + Fast extension

- **Status**: accepted
- **Date**: 2026-04-13
- **Deciders**: magpie maintainers

## Context

A peer connection has two orthogonal axes of state — *choke* (whether the
remote is willing to service our requests) and *interest* (whether each side
wants to download from the other). BEP 6 (Fast extension) layers a third
concept on top: the *allowed-fast set* lets a peer service requests for a
fixed subset of pieces even while choking us. Magpie needs an explicit policy
for how these interact in the M1 leecher.

## Decision

The session models peer state as plain fields on a per-peer struct
(`session::torrent::PeerState`):

```
struct PeerState {
    choking_us: bool,       // remote → us  (start: true per BEP 3)
    we_are_interested: bool, // us → remote (start: false; flipped via SetInterested)
    in_flight: u32,         // count of issued requests not yet answered
    have: Vec<bool>,        // remote's bitfield, mirrored from Bitfield/Have/HaveAll/HaveNone
    ...
}
```

Transitions:

- **On `Bitfield` / `Have` / `HaveAll` / `HaveNone`**: update `have` and
  re-evaluate interest. Send `Interested` / `NotInterested` to flip our state.
- **On `Choke`**: set `choking_us = true`, drop the in-flight counter, and
  release every `claimed` block back to the picker. If the *Fast extension was
  not negotiated*, the peer task also clears its local `in_flight` set
  because BEP 3 says all outstanding requests are implicitly cancelled when
  the peer chokes us.
- **On `Unchoke`**: set `choking_us = false` and let the scheduler hand it
  more work.
- **`AllowedFast(p)` / `SuggestPiece(p)`**: stored as hints in M1 but not yet
  promoted in the picker — Phase 4/5 wires them into priority ordering.
- **`RejectRequest(req)`**: peer task removes `req` from its in-flight set;
  session releases the block claim. We do not penalise the peer in M1.

The codec is stateless and accepts BEP 6 messages regardless of the
handshake's reserved bits. **The session enforces gating**: when a peer
sends a BEP 6 message and the handshake didn't negotiate Fast, the session
will (Phase 4 hardening) drop the connection with a protocol-violation
alert. Today the M1 prototype tolerates them.

## Consequences

Positive:

- Boring, debuggable. No state-machine framework, no enum-with-thirty-arms.
- Aligns with anacrolix and librqbit shapes — divergence costs explainability
  for new contributors.
- Cleanly extends to seeding (M2) by adding `am_choking_them` /
  `they_are_interested` fields without changing the existing transitions.

Negative:

- Drift between codec (stateless, accepts BEP 6 unconditionally) and session
  (must gate on negotiated Fast bit) is a maintenance hazard. Mitigation: the
  `Message` enum rustdoc explicitly calls out the caller's BEP 6 gating
  responsibility (W4 from the phase 1+2 red-team review).

## Alternatives considered

- A typed state-enum (`enum PeerState { Choked, Unchoked, ChokedFastAllowed }`)
  with explicit transitions — rejected. Adds visual ceremony without removing
  any actual decision points. Bit fields scale better when the seeder side
  lands.
- Pushing BEP 6 gating into the codec — rejected. The codec doesn't see the
  handshake; making it stateful for one feature corrupts the layering.
