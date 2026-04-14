# 0005 — Piece-picker architecture

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: research/SUMMARY.md (anacrolix B-tree recommendation), M1 `crates/magpie-bt-core/src/picker.rs`, rasterbar piece-picker pain notes (research/003 §2, §5)

## Context

`docs/research/SUMMARY.md` §2 pencilled in an anacrolix-style B-tree keyed on `(availability, priority, partial)` for the piece picker, based on a pre-code reading of three reference implementations. By M2 kickoff, M1 has shipped a different shape: a linear rarest-first + endgame picker (`Picker` in `crates/magpie-bt-core/src/picker.rs`, 391 LoC) whose `pick()` is explicitly documented as O(piece_count) and "comfortable up to ~10k pieces". Same situation as ADR-0004 — the research recommendation was written for an empty codebase; the shipped code made a different trade-off.

M2 adds seed-side piece selection, so this ADR needs to settle two questions together:

1. Do we migrate to the B-tree now, or keep the linear scan through M5?
2. Where does seed-mode piece selection live — in `Picker`, or elsewhere?

### What M2 actually asks of the picker

- **Leech-mode rarest-first**: already shipped.
- **Endgame at low missing-fraction**: already shipped.
- **Seed-mode "can I serve this?"**: binary lookup `have[piece]`. Not a *selection* problem — the peer's `Request` names the piece, we check if we have it.
- **Partial-piece affinity** (prefer finishing a piece in progress): nice-to-have, not a correctness item; defers.
- **Extent / speed-class affinity**: already scheduled M5 (`ROADMAP.md`).
- **Priorities**: not in M2 scope.

### Cost model

M1's linear scan does one O(n) pass per `pick()`. A 10k-piece torrent (Debian ISO: 3016 pieces; typical large archive: 10k–100k) at 100 picks/s is 1 M comparisons/s — not visible on a flamegraph. A 1 M-piece torrent (≈256 GiB at 256 KiB pieces, ≈4 TiB at 4 MiB) would be 100 M/s, which starts to matter. That workload is rare, and our primary consumer (`lightorrent` serving *arr workflows) does not encounter it.

### Soak-workload selection (gate-tied note)

The M2 Gate criterion #3 (24 h multi-torrent soak, ≥8 concurrent torrents) is where the cost model gets empirically tested. **The soak torrent set must include at least one large-piece-count torrent** (target: ≥100k pieces, ≈25 GiB at 256 KiB pieces) so the picker's O(n) path is actually exercised. A soak built entirely from Debian-class torrents (3k pieces each) leaves the picker below the "flamegraph visible" threshold, which makes the migration trigger unfireable and the cost model unvalidated. This is a deliberate gate-design constraint, not a scope item: the soak is how we learn whether the linear scan holds up, and it only teaches us that if the workload reaches the interesting range.

## Decision

**Keep the M1 linear rarest-first picker through at least M5.** Defer the anacrolix B-tree migration to M5+ when (a) priority / speed-class work needs it, or (b) profiling shows `pick()` on the flamegraph for a realistic workload. Same reasoning as ADR-0004: the shipped trade-off works for M2–M5 workloads, and the plan-worshipped B-tree pays off only when something M2 does not need pays the complexity cost.

### Scope boundaries

The `Picker` is **piece-granular and leech-only**:

- `Picker` tracks per-piece availability + have, nothing else. No block tracking, no per-piece request state, no partial-progress accounting.
- **Per-block state lives in `TorrentActor`**, in the `in_progress: HashMap<PieceIndex, InProgressPiece>` map already shipped in M1 (ADR-0007). Block claims, reject/cancel, endgame-redundant-request suppression — all `TorrentActor` concerns.
- **Seed-mode is passive**: when a peer sends `Request { piece }`, the upload path (ADR-0017) checks `have[piece]` and either reads or rejects. The `Picker` is never consulted.
- **`Have` bitfield** is the ground truth for "can we serve piece X?". `Picker::has_piece(piece)` is the existing query; seed-mode uses the same accessor.

### Peer need-set (ADR-0020 hook)

`SeedChoker` ranking (ADR-0012) wants to know which pieces each peer *needs*. This is the complement of the peer's advertised bitfield against ours — but it does **not** belong in the `Picker`:

- The picker's job is to choose what **we** should download next. Per-peer need-sets are seed-side metadata.
- Store the peer's advertised bitfield on `PeerState` (ADR-0009 already has `have: Vec<bool>`; upgrade to `BitVec` for bitwise ops). Compute the need-set on-demand at SeedChoker read time as `our_have & !peer.have` — one bitfield AND per peer per choker tick, trivially cheap (O(pieces / 64) 64-bit ops; a 100k-piece bitfield is 196 `u64` AND operations per peer).
- **Do not cache need-sets.** An earlier draft cached a per-peer need-set updated incrementally on `Have` / `Bitfield` / `HaveAll` / `HaveNone` from the peer and on local `mark_have`. That design has a correctness trap: at the leech→seed transition (ADR-0019), `our_have` flips in one shot from partial to complete, and every peer's cached need-set becomes stale for up to one choker tick — the SeedChoker ranks against the pre-completion view, slightly skewing the first round's unchoke decisions. On-demand computation sidesteps this entirely; completion just changes `our_have` and the next read reflects it. The ADR-0019 completion transition does not need a need-set fix-up step because there is no cached state to invalidate.
- `BitVec` for M2 simplicity; `RoaringBitmap` deferred until the bitfield operations show up on flamegraphs (not expected at M2 peer counts).

This keeps `Picker` narrow and `PeerState` seeded for ADR-0020.

### Partial-piece affinity — deferred

Tempting to add "prefer finishing a piece that's N% complete" to the picker's tie-break. Deferred to M5 alongside extent affinity because (a) the linear picker makes it cheap to add later (one extra tie-break in the same scan), (b) no M2 gate criterion depends on it, and (c) adding it now requires threading partial-progress data into `Picker` which today has none.

### Migration trigger for the B-tree

Concrete, not vague:

- **`pick()` appears in flamegraphs** of a realistic workload (not a 1 M-piece stress test), **or**
- **Priority / speed-class affinity work lands** (M5), which wants an indexed structure anyway.

Either of those, we migrate; neither, we don't.

## Consequences

Positive:

- Zero M2 churn in the picker. Existing leech path, tests, and BDD coverage keep working.
- Clean separation: `Picker` = leech selection; `TorrentActor` = per-block state; `PeerState` = per-peer need-set; seed-mode = passive `has_piece()` lookup. Each concern has one home.
- M5 migration is additive: the B-tree replaces the linear scan inside `Picker::pick()` without changing callers. `has_piece()`, `mark_have()`, `observe_peer_bitfield()`, and `forget_peer_bitfield()` are the same.
- Keeps the door open for ADR-0020 super-seeding (M6+) — it will need per-peer need-sets and per-piece-upload-count metadata, both of which sit outside `Picker`.

Negative:

- Large-torrent performance is bounded by `pick()` at O(piece_count). A 1 M-piece torrent pays 100 M comparisons/s at 100 picks/s. We accept this for M2–M5 based on the workload mix, with an explicit migration trigger above.
- No partial-piece affinity until M5. On slow, churny swarms, magpie will accumulate more partial pieces than rasterbar or librqbit at the same completion fraction. Visible in flamegraphs of the torrent state but not in user-visible throughput on typical workloads.

Neutral:

- Seed-mode piece selection being passive means the picker has no seed-side code path. If super-seeding (BEP 16, out of scope through M6) is eventually implemented, it lives in a new `SeedPicker` or a mode flag on the torrent actor — `Picker` itself doesn't need restructuring.

## Alternatives considered

- **Migrate to anacrolix B-tree in M2** (the pencil-in from SUMMARY.md). Rejected: same reasoning as ADR-0004 — the migration buys us nothing M2 measurably needs, costs implementation time, and the M5 trigger is clean. Keeps magpie shipping features over shipping restructures.
- **Separate `SeedPicker` type.** Rejected: seed-mode selection is passive. Adding a type to represent "look up a bit in a bitfield" is ceremony without information gain. If super-seeding lands we revisit.
- **Put per-peer need-sets in `Picker`.** Rejected: need-sets are seed-mode metadata and live on `PeerState`. Conflating them into `Picker` (which is about "what should *we* download") mixes concerns and complicates the M5 B-tree migration.
- **Add partial-piece affinity now.** Rejected: no M2 gate depends on it; the linear scan makes it cheap to add when wanted; threading partial-progress data into `Picker` is new state surface we don't need yet.
- **Roaring bitmaps for need-sets now.** Rejected for M2: a 100k-piece `BitVec` is 12.5 KiB per peer — fine at the per-session peer cap. Roaring pays off at 1 M+ pieces or when bitfield-bitfield ops are frequent. Revisit if ADR-0012 seed-ranking needs heavy bitfield intersection on the hot path.
- **Cache need-sets per peer, update incrementally.** First draft of this ADR. Rejected: the leech→seed transition (ADR-0019) flips `our_have` in one shot; every cached need-set becomes stale for up to one choker tick, skewing the first post-completion unchoke round. On-demand `our_have & !peer.have` costs ~200 u64 ops per peer per 10 s tick — invisible — and has no staleness trap. Simpler and correct.
