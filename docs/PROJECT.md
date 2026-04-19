# magpie — Project Definition

Stable *why/what* for the project. For sequencing and status see [ROADMAP.md](ROADMAP.md) and [MILESTONES.md](MILESTONES.md).

- **Repo**: `github.com/mlamp/magpie`
- **Crate prefix**: `magpie-bt-*` on crates.io (bare `magpie` is taken by an unrelated Othello library)
- **Reference consumer**: [lightorrent](../../lightorrent) — used as a design sanity check for API completeness ("does magpie cover real call sites?"). Not a milestone gate: consumer adoption happens in that repo, on its timeline. magpie is a general-purpose library.

## Motivation

Lightorrent runs on `librqbit 8`. It works, but has gaps that force workarounds:

- No persistent upload/download stats (counters reset on restart/pause).
- No event/messaging system for piece-level activity — we poll instead.
- Useful types are `pub(crate)` and can't be named in consumer code.
- Session state file duplicates what we track in redb.
- Peer-ID prefix is hardcoded to `-rQ????-`, unusable for private-tracker client whitelisting.

Rather than fork librqbit, build a new library (magpie) designed from a consumer's perspective — ours — with a clean event bus, configurable identity, and first-class BEP 52 data model. Lightorrent keeps running on librqbit until magpie reaches parity.

## Non-goals

- Not a librqbit drop-in replacement API. The public surface is designed fresh.
- Not v2-only. See BEP strategy below.
- Not runtime-agnostic. Tokio-only; reconsider if benchmarks demand it.

## Historical motivation

The detailed gap analysis against librqbit 8 that originally justified this project lives in [archive/librqbit-gap-analysis.md](archive/librqbit-gap-analysis.md). It is archived rather than inline so PROJECT.md stays evergreen; requirements derived from those gaps are folded into the architecture principles below.

## BEP strategy

**v1 on the wire, v2-aware data model from day one, hybrid support in M4. Not v2-only.**

Rationale: BEP 52 has been final since 2020 but adoption in the wild is negligible — public and private trackers, *arr workflows, and existing .torrent files are overwhelmingly v1. A v2-only client can't meaningfully participate in existing swarms. v2 is additive to v1, not a replacement, so writing v1 is ~90% of writing v2. Abstract the hash layer early (`PieceHash::{V1, V2}`), enforce v2 block/piece-size invariants (16 KiB blocks, power-of-two piece sizes) even in v1, and v2/hybrid slots in without a rewrite.

**Day-one BEPs** (by end of M5): 3, 6, 9/10, 12, 15, 23, 27, 29. Per-milestone split: BEPs 3, 6, 23 land in M1; 12, 15, 27 in M2; 9/10/11/14 in M3 (magnet + extension protocol + PEX + LSD); 5 in M4 (DHT); 29 in M5 (uTP).
**Later**: 52 (v2/hybrid — M5), 19 (WebSeed — M6), 48 (scrape — M6).

## Architecture principles

- **Actor-owned piece state.** One task owns picker + state per torrent. Peers talk to it via channels. No `Arc<Mutex<Session>>` god-object (the pattern librqbit drifted into).
- **Event bus, not polling.** `TorrentEvent` enum on broadcast; bounded MPSC for any high-volume per-block stream.
- **Storage is a trait.** Impls: file (default), mmap, in-memory (tests). Later: sqlite, S3.
- **Disk I/O never on the runtime.** Dedicated pool, bounded queue, metrics on queue depth. Vectorised `pwritev`/`preadv` on Unix.
- **Every BEP is a module**, not conditional spaghetti through the core.
- **Feature flags for optional protocols** (`dht`, `utp`, `v2`, `webseed`) so small builds stay small.
- **No allocations in the piece/request hot loop.** Preallocate request queues, reuse block buffers.
- **Typed errors per module** (`thiserror`). No `Box<dyn Error>` in hot paths.
- **Public API is client-agnostic; lightorrent's call sites are a completeness check, not the shape driver.** If a realistic BitTorrent client would need to reach into internals, that's an API bug in magpie.

## Crate layout

Workspace under `magpie`:

- `magpie-bt-bencode` — zero-copy bencode codec (`Cow<[u8]>`).
- `magpie-bt-metainfo` — .torrent parsing: v1, v2, hybrid. Hash abstraction.
- `magpie-bt-wire` — peer protocol codec (framing, messages, extension protocol).
- `magpie-bt-core` — engine: picker, storage trait, event bus, session orchestration.
- `magpie-bt-dht` (M4), `magpie-bt-utp` (M5) — optional feature-gated subcrates.
- `magpie-bt` — facade crate re-exporting the public API.

## Inspiration (what to borrow and from where)

| Pattern | Source | Why |
|---|---|---|
| Alert/event ring buffer | libtorrent-rasterbar | Typed, non-blocking, subscribe-once |
| Rarity-sorted picker + speed-class affinity | libtorrent-rasterbar | Less partial-piece waste on churn |
| Disk I/O pool off the net loop | libtorrent-rasterbar | Net never stalls on fsync |
| Pluggable storage trait | anacrolix/torrent | Future-proofs mmap/sqlite/S3 |
| Vectorised `pwritev`/`preadv` | cratetorrent | Fewer syscalls at gigabit |
| Userspace uTP + metrics | rakshasa, rasterbar | No kernel dep, observable (librqbit has no uTP in current tree — see [research](research/SUMMARY.md)) |
| Azureus peer-ID builder | MonoTorrent | Configurable client identity |
| Single-threaded epoll aesthetic | rakshasa | "Lean and mean" discipline |
| Merkle layer peer-fetch | MonoTorrent, libtorrent 2.x | Correct BEP 52 when metadata omits layers |
| `io.Reader`-style per-file access | anacrolix/torrent | Streaming falls out naturally |
