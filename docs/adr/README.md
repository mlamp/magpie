# Architecture Decision Records

One record per non-trivial design decision. Format: [Michael Nygard ADR](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).

## How to add an ADR

1. Copy the skeleton below into `NNNN-short-title.md`, where `NNNN` is the next 4-digit number.
2. Status starts as `proposed`. Move to `accepted`, `superseded by NNNN`, or `rejected` as the decision lands.
3. Link the ADR from the relevant milestone file or code comment when the decision is implemented.

### When an ADR is required

- Before introducing `unsafe` to a new crate.
- Before adding a dependency with a non-standard license or a transitive advisory exception.
- Before taking a hard runtime/ecosystem commitment (e.g. tokio-only, specific crypto library).
- Before any design decision future contributors will need to ask "why?" about in six months.

## Skeleton

```markdown
# NNNN — Short title

- **Status**: proposed | accepted | rejected | superseded by NNNN
- **Date**: YYYY-MM-DD
- **Deciders**: <names or handles>
- **Consulted**: <optional>

## Context

What is the forcing function? What constraints apply? What did we know at decision time?

## Decision

What we are going to do. Declarative, not negotiable within this ADR.

## Consequences

Positive, negative, and neutral. What becomes easier? What becomes harder? What assumptions does this bake in?

## Alternatives considered

Short notes on options we did not pick, and why.
```

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-subcrate-vs-feature-dht-utp.md) | Subcrate vs. feature for DHT and uTP | accepted |
| [0002](0002-event-bus-alert-ring.md) | Event bus: custom rasterbar-style alert ring | accepted |
| [0003](0003-tokio-only.md) | Tokio-only runtime | accepted |
| [0004](0004-storage-trait-shape.md) | Storage trait shape (flat positional `&self`; `PieceHandle` migration deferred to M6) | accepted |
| [0005](0005-picker-architecture.md) | Picker: keep M1 linear rarest-first + endgame; B-tree migration deferred to M5+ | accepted |
| 0006 | v1/v2 hash data model (`PieceHash`/`InfoHash` enums) | open, to be drafted in M0 |
| [0007](0007-disk-write-backpressure.md) | Disk-write backpressure (bounded queue) | accepted |
| [0008](0008-vectorised-file-io.md) | Vectorised file I/O for `FileStorage` (`pwritev`/`preadv`) | proposed (placeholder) |
| [0009](0009-peer-state-machine.md) | Peer connection state machine + Fast extension | accepted |
| [0010](0010-request-pipelining.md) | Request pipelining + endgame | accepted |
| [0011](0011-tracker-http-rustls.md) | Tracker HTTP transport: `reqwest` + `rustls-tls` | accepted |
| [0012](0012-choker.md) | Choker: leech tit-for-tat + seed fastest-upload; enum-switched modes | accepted |
| [0013](0013-bandwidth-shaper.md) | Bandwidth shaper: three-tier token buckets (session / torrent / peer), six total per session | accepted |
| [0014](0014-stats.md) | Stats: per-peer cumulative atomics, 1 Hz event, pluggable `StatsSink` | accepted |
| [0015](0015-udp-demux.md) | UDP demux: one socket, first-byte dispatch, transaction-id tracker routing | accepted |
| [0016](0016-engine-abstraction.md) | Engine abstraction: `TorrentEngine` trait in lightorrent, two adapters | accepted |
| [0020](0020-peer-need-set.md) | Peer need-set (pointer to ADR-0005 §Peer need-set) | accepted |
| [0017](0017-upload-request-flow.md) | Upload request flow: per-peer unread/ready queues + send-buffer watermark | accepted |
| [0018](0018-read-cache.md) | Read cache: session-global piece-granular LRU with store-buffer short-circuit | accepted |
| [0019](0019-completion-transition.md) | Leech→seed completion transition: five-step forward-only sequence | accepted |
| [0021](0021-multi-file-storage.md) | Multi-file storage: `MultiFileStorage` impl of the existing `Storage` trait, sorted entries + bounded LRU fd pool | proposed |
