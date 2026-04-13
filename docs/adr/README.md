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
| [0001](0001-subcrate-vs-feature-dht-utp.md) | Subcrate vs. feature for DHT and uTP | proposed (research done) |
| [0002](0002-event-bus-broadcast.md) | Event bus on tokio::sync::broadcast | proposed (research done) |
| [0003](0003-tokio-only.md) | Tokio-only runtime | proposed (research done) |
| 0004 | Storage trait shape (anacrolix-style PieceHandle) | open, to be drafted in M0 |
| 0005 | Piece-picker architecture (B-tree + availability key) | open, to be drafted in M0 |
| 0006 | v1/v2 hash data model (`PieceHash`/`InfoHash` enums) | open, to be drafted in M0 |
| 0007 | Disk-write backpressure (bounded queue) | open, to be drafted in M0/M2 |
