# 0028 — Metadata exchange flow (BEP 9, `ut_metadata`)

- **Status**: accepted
- **Date**: 2026-04-21 (post-hoc acceptance at M3 close)
- **Deciders**: magpie maintainers
- **Consulted**: BEP 9 § "ut_metadata", ADR-0027 (extension registry), rasterbar `ut_metadata` impl, librqbit metadata flow

## Context

Magnet links (ADR-less consumer concern, BEP 10-dependent) supply
only the info-hash; the torrent's `info` dict — piece hashes, file
layout, piece length — has to be fetched from peers via the
`ut_metadata` extension. BEP 9 splits the info dict into 16 KiB
pieces and transfers them over the BEP 10 extension protocol. The
receiver reassembles, hashes, and verifies against the info-hash
before transitioning the torrent into "downloading" state.

Design decisions needed:
- Piece size (fixed by BEP 9 at 16 KiB) — no magpie decision.
- How is the assembled metadata verified?
- What happens on hash mismatch — retry, ban peer, or give up?
- Bounds on piece count / metadata size to stop malicious peers
  from forcing us to allocate gigabytes.
- When does the torrent transition from "fetching metadata" to
  "downloading"?

## Decision

### Reassembly

`crates/magpie-bt-core/src/session/metadata_exchange.rs` owns
`MetadataAssembler`: a per-torrent accumulator keyed on
`(piece_index, 16 KiB block)`. Requests are issued to one peer at
a time (the first peer to advertise `metadata_size` in its BEP 10
handshake) until the peer rejects or times out, then the next
advertising peer is tried.

Pieces arrive out of order; once every piece is present, the
concatenation is SHA-1-hashed and compared against the torrent's
info-hash. On match, the bytes are parsed into `TorrentParams`
(piece hashes, file list, piece length). On mismatch, **all pieces
are discarded and the assembler restarts from scratch** —
individual-piece re-requests can't resolve a corrupted set without
knowing which piece was wrong, and the whole set is 16 KiB × N
pieces ≈ small enough that full restart is the simplest
correctness-preserving strategy.

### Bounds

- `MAX_METADATA_SIZE = 16 MiB`. A torrent's info dict is almost
  always a few KB to a few MB; 16 MiB is 3 orders of magnitude of
  headroom while refusing multi-GB crafted payloads. Advertised
  `metadata_size` values above this cap are rejected at handshake
  time (ADR-0027).
- `MAX_PIECE_COUNT = 2_097_152` (2 Mi). At 16 KiB per piece this
  is a hard upper bound of 32 GiB of metadata; together with the
  above size cap, it bounds per-piece bookkeeping even if a peer
  advertises a small `metadata_size` but actually sends a
  different count.
- `MAX_PIECE_LENGTH = 64 MiB`. Defensive parse-time check on the
  decoded `info.piece length` field after SHA-1 succeeds —
  arbitrary-sized pieces are a denial-of-service vector for the
  downstream piece picker.

### Retry policy

- Per-piece: on timeout, re-request from the same peer up to 3
  times, then fall through to the next advertising peer.
- Per-session: on hash-verification failure, restart the assembler
  fresh. After **3 consecutive full-restart failures** the torrent
  session emits `Alert::Error { code: MetadataVerifyExhausted }`
  and gives up. A hostile peer chain that always hands us
  corrupted metadata can't burn our CPU indefinitely.

The 3-failure ceiling is enforced by `TorrentSession` (the caller
of `MetadataAssembler`); the assembler itself exposes verification
results to let consumers wire alternative policies, but the
shipped torrent session enforces it.

### State transition

- Torrent starts in `State::FetchingMetadata` when added via
  `Engine::add_magnet`.
- On successful verify + parse, `Alert::MetadataReceived` fires,
  `TorrentParams` is installed, and the torrent transitions to
  `State::Downloading`. From here it's indistinguishable from a
  torrent added with a full `.torrent` file.
- Peers that sent us metadata pieces are kept connected — they're
  part of the swarm now.

## Consequences

Positive:
- Magnet links work end-to-end with `Engine::add_magnet(magnet_uri)`;
  no separate "metadata fetch" API the consumer has to drive.
- Bounds on every allocation surface stop malicious peers from
  OOMing us during the metadata phase.
- Hash-verify-then-parse ordering means malformed-bencode attacks
  are blocked unless the attacker also forges a SHA-1 collision
  with the info-hash.

Negative:
- Full-restart on hash mismatch throws away possibly-valid pieces.
  Acceptable trade-off: pieces are 16 KiB each, the full set is
  small, and partial-trust recovery is its own hardening project.
- 3-failure ceiling is terminal. A consumer wanting "try forever"
  can subscribe to `MetadataVerifyExhausted` and re-add via magnet
  with different initial peers.

Neutral:
- `MetadataAssembler` is usable standalone (no failure ceiling
  built in); the ceiling lives at the session layer. Consumers
  using the assembler directly must enforce their own policy.

## Alternatives considered

- **Per-piece hash verification during reassembly**. Rejected:
  BEP 9 hashes the whole metadata, not per-piece; there's no
  per-piece hash to check against.
- **Parallel metadata fetch from all peers**. Rejected: adds
  bandwidth + bookkeeping complexity for no latency win — the
  info dict is small and a single peer's throughput saturates
  trivially.
- **Persist partial metadata across restarts**. Rejected:
  magnet-initiated restarts would have to reconcile with a stale
  partial set that may have been wrong. Fresh fetch is simpler
  and adequately fast.
