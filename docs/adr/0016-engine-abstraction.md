# 0016 — Engine abstraction in lightorrent

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: lightorrent `src/engine.rs` (M1 shape, hard-wired to `librqbit::Session`), PROJECT.md principle "Public API designed from lightorrent's call sites first", ROADMAP.md M5 (librqbit removal)

## Context

M2 dogfoods magpie inside lightorrent via a `--engine=magpie` flag. Lightorrent's current `Engine` struct (`lightorrent/src/engine.rs`, ~400 LoC) is a thin wrapper around `Arc<librqbit::Session>` plus lightorrent-specific state (redb `TorrentRegistry`, stats baselines, ratio task, cancel token). Every qBittorrent-compatible API handler in `src/api.rs` (~1 000 LoC) goes through the `Engine` to reach the underlying session. To swap engines, we need an abstraction that both `librqbit` and `magpie` implement — and that abstraction must not distort either backend.

Two constraints pull in opposite directions:

1. **PROJECT.md**: *"Public API designed from lightorrent's call sites first. If lightorrent needs to reach into internals, that's an API bug in magpie."* → the abstraction must be shaped by lightorrent's needs, not copied from librqbit.
2. **Ship without disturbing librqbit 8**: the abstraction must cover everything lightorrent currently does through librqbit, or lightorrent breaks in the `--engine=librqbit` default path.

### M5 exit trajectory

ROADMAP.md M5 removes librqbit. At that point the trait becomes load-bearing for nobody — there's only one backend. The design should let us delete the trait in M5 without rewriting call sites: `api.rs` stays, `magpie.rs` graduates to `engine.rs`, `librqbit.rs` and the trait itself disappear.

## Decision

### Where the trait lives

**In `lightorrent/src/engine/`**, not in magpie. Module layout:

```
lightorrent/src/engine/
├── mod.rs          // pub trait TorrentEngine, plus helper types
├── librqbit.rs     // impl TorrentEngine for LibrqbitEngine { .. }
└── magpie.rs       // impl TorrentEngine for MagpieEngine { .. }
```

The `Engine` struct in lightorrent changes from "thin wrapper over librqbit" to "thin wrapper around `Arc<dyn TorrentEngine>`". Lightorrent-specific state (`TorrentRegistry`, `StatsBaseline`, cancel token) stays on the outer `Engine`, outside the trait.

Trait lives in lightorrent because:

- **It's lightorrent's shape**. The trait is defined by what `api.rs` needs, not by what either backend ships.
- **Neither backend should import from lightorrent** to implement the trait of its own consumer. Both `LibrqbitEngine` and `MagpieEngine` are thin adapters inside lightorrent that wrap the backend's native API into the lightorrent-defined trait.
- **M5 deletion is local.** When librqbit is removed, we delete `librqbit.rs`, collapse `magpie.rs` into `engine.rs`, remove the trait. No changes in magpie; no changes in `api.rs`.

### Trait surface (by call site)

Derived from reading every call in `api.rs` and `engine.rs`. All methods are async (`async_trait` or Rust 2024 native async fn in trait — native preferred since our MSRV is 1.94). Object-safe required because lightorrent holds `Arc<dyn TorrentEngine>`.

```rust
#[async_trait]
pub trait TorrentEngine: Send + Sync {
    // Lifecycle
    async fn shutdown(&self);

    // Torrent CRUD
    async fn add_torrent(&self, src: TorrentSource, opts: AddOpts)
        -> Result<AddResult, EngineError>;
    async fn remove_torrent(&self, id: &TorrentHandle, delete_files: bool)
        -> Result<(), EngineError>;
    async fn pause(&self, id: &TorrentHandle) -> Result<(), EngineError>;
    async fn resume(&self, id: &TorrentHandle) -> Result<(), EngineError>;

    // Introspection
    fn list_torrents(&self) -> Vec<TorrentHandle>;  // sync snapshot
    async fn torrent_info(&self, id: &TorrentHandle) -> Result<TorrentInfo, EngineError>;
    async fn files(&self, id: &TorrentHandle) -> Result<Vec<FileInfo>, EngineError>;
    async fn stats(&self, id: &TorrentHandle) -> Result<TorrentStats, EngineError>;

    // Events (replaces the M1 poll loop)
    fn subscribe_stats(&self) -> mpsc::Receiver<StatsUpdate>;
    fn subscribe_lifecycle(&self) -> mpsc::Receiver<LifecycleEvent>;
    // `LifecycleEvent::Shutdown` is guaranteed to be the final message on every
    // lifecycle receiver before the adapter's forwarding tasks exit. Stats
    // receivers close after lifecycle's Shutdown fires; a consumer holding
    // both channels should treat the Shutdown as the authoritative "engine
    // gone" signal and stop processing downstream stats. Without this,
    // consumers have only channel-closed EOF, which doesn't distinguish
    // "engine shut down" from "I fell behind and got dropped" — silently
    // halting a ratio enforcer, for example.

    // Backend-specific introspection (optional; both may return empty)
    async fn dht_node_count(&self) -> Option<u32>;
}
```

Shared types (`TorrentHandle`, `TorrentSource`, `AddOpts`, `TorrentInfo`, `TorrentStats`, `StatsUpdate`, `LifecycleEvent`, `FileInfo`, `EngineError`) live in `lightorrent/src/engine/mod.rs` and are lightorrent-owned. Each backend adapter maps the native types (librqbit's or magpie's) into these shared types.

### Mapping to magpie's public API

Each trait method maps to one or more calls on magpie's facade crate (`magpie-bt`). Call-site examples (not exhaustive):

- `add_torrent` → `magpie::Session::add_torrent(AddTorrentBuilder...)`
- `subscribe_stats` → `magpie::Session::subscribe::<StatsUpdate>()` from the alert ring (ADR-0002, ADR-0014). Lightorrent remaps `magpie::StatsUpdate` → `lightorrent::StatsUpdate` in the adapter.
- `stats(id)` → `magpie::Session::torrent_stats(info_hash)` — reads live atomics per ADR-0014's public-API contract.
- `shutdown` → `magpie::Session::shutdown()` which calls `StatsSink::flush` with the `flush_timeout` from ADR-0014.

**This is the authoritative list of things magpie's facade must expose.** Any lightorrent call site that can't be served by a magpie public API is an API bug in magpie, per PROJECT.md. Filing those bugs is part of the M2 integration workstream.

### Dispatch cost

API handlers run per HTTP request; `stats(id)` runs at whatever cadence lightorrent's ratio enforcement uses. Virtual dispatch through `Arc<dyn TorrentEngine>` costs ~1 ns per call. At the highest plausible rate (a browser polling `/torrents/info` at 10 Hz with 100 torrents = 1 000 calls/sec), that's 1 µs/sec of dispatch overhead — genuinely invisible. The ADR-0016 draft note earlier in the milestone said "negligible — not zero"; this is where the distinction matters honestly.

### CLI flag + runtime selection

```
lightorrent --engine=librqbit  (default in M2)
lightorrent --engine=magpie
```

`main.rs` matches on the flag and constructs either `LibrqbitEngine::new(config).await?` or `MagpieEngine::new(config).await?`, both returning `Arc<dyn TorrentEngine>`. The outer `Engine` wrapper holds this `Arc` and is engine-agnostic.

CI runs the full integration suite (`tests/integration.rs`) under both values of `--engine`. M2 gate #6 requires both be green.

## Consequences

Positive:

- **Lightorrent's current code mostly survives.** `api.rs` changes at call sites that previously reached into librqbit-specific types; everything else stays.
- **Magpie's public API is pulled by demand, not guessed.** Any method the trait needs becomes a magpie API requirement. This is the PROJECT.md contract enforced mechanically.
- **M5 librqbit removal is local.** Delete two files, inline one, remove the trait. Call sites in `api.rs` don't move.
- **Both engines run side-by-side in CI.** M2 gate is a real regression harness, not a one-time integration check.
- **Backends can't distort each other.** Adapter files map native types to shared lightorrent types; neither backend cares about the other.

Negative:

- **Adapter code is duplicative.** `librqbit.rs` and `magpie.rs` both contain type-translation boilerplate (native → shared). Until M5 this is real maintenance cost; after M5 it's gone.
- **The trait surface is snapshot-shaped.** It captures what `api.rs` needs today. A new lightorrent feature that wants functionality neither backend exposes requires a trait extension + implementations in both. Manageable; the trait is lightorrent-owned so changes don't require magpie version bumps.
- **`subscribe_stats` and `subscribe_lifecycle` return `mpsc::Receiver`** — a single subscriber per call. Multiple subscribers (e.g. ratio enforcer + UI stats feed) need a broadcast fan-out on lightorrent's side. Lightorrent already has this pattern; adapters just surface one receiver per call.
- **Object-safe async trait constraints** bind us away from `impl Future` return positions in some places. `async_trait` handles it today; native async-fn-in-trait is the M5-era cleanup.

Neutral:

- **Trait lives in lightorrent, not magpie.** Consequence: magpie has no `TorrentEngine` trait to document; magpie exposes concrete types. This is correct per PROJECT.md but worth noting for anyone browsing magpie's crate docs looking for an "engine trait."

## Alternatives considered

- **Trait in magpie**, library-owned. Rejected: forces magpie's public API into a shape dictated by the trait, which in turn is dictated by one consumer's needs. Every future magpie consumer would inherit lightorrent's shape. Library-owned abstractions work when there are many consumers; magpie has one.
- **Mirror librqbit's `Session` API on magpie**, drop-in at import site. Rejected: magpie's API would be constrained by librqbit's choices through M5, then would need renaming at M5 to reflect its own identity. Churn cost exceeds the trait's adapter cost.
- **Generics instead of `Arc<dyn TorrentEngine>`** (`Engine<E: TorrentEngine>`). Rejected: `main.rs` picks the engine at runtime from a CLI flag; runtime dispatch is the match. Monomorphisation would double the compile output and code size for no runtime win.
- **No abstraction; feature flag between backends at compile time.** Rejected: can't run the CI matrix side-by-side, can't ship a single lightorrent binary that supports both. The CLI flag shape was the M2 gate's explicit choice.
- **Split the trait into smaller traits** (`AddTorrent`, `Lifecycle`, `Stats`, etc.). Rejected for M2: one trait is easier to reason about, and the backends both cover the whole surface. Split if a future consumer needs only a subset.
- **Native `async fn` in trait without `async_trait`.** Rust 1.94 supports it, but object-safety constraints for `async fn` in trait still need care (return-position `impl Trait` in public traits can trip up dyn-safety). `async_trait` is the path of least resistance for M2; migrate post-stabilisation if it simplifies.
