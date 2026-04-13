# 004 — librqbit

- **Repo**: https://github.com/ikatson/rqbit
- **Commit**: `f9b4aee85aff0fe52e206cfa3d3d5cc7e7d24947`
- **Date**: 2026-04-13

## Session lifecycle

Not an `Arc<Mutex<Session>>` god-object in the current tree (historical claim refuted — see §8). Structure:

- `Session::new()` / `Session::new_with_opts()` returns `Arc<Self>` (`crates/librqbit/src/session.rs:502-512`).
- `add_torrent(AddTorrent, opts)` parses metadata, builds `ManagedTorrent` wrapped in `Arc`, stores in `db: RwLock<SessionDatabase>` (`session.rs:109`). Returns `AddTorrentResponse::Added(TorrentId, Arc<ManagedTorrent>)`.
- State machine (`torrent_state/mod.rs:67`): `Initializing → Paused ↔ Live`, any state → `Error`. Each variant wraps a dedicated struct (`TorrentStateInitializing`, `TorrentStatePaused`, `TorrentStateLive`).
- Peer spawning lives in `TorrentStateLive`, which merges streams from DHT / trackers / manual adds and enforces peer limits via `Semaphore` (`limits.rs`).
- Shutdown: `CancellationToken` + `DropGuard` on `Session` (`session.rs:125-126`) cascade cancellation through all tasks.

## Event / progress exposure — polling

**No event bus.** Consumers poll.

- `api_stats_v1(torrent_id)` (`api.rs:496-498`) calls `ManagedTorrent::stats()` which recomputes from current state on demand (`torrent_state/mod.rs:472-531`). No caching, no notification.
- Log lines have a broadcast stream (`api.rs:361-374`), but there is **no** `TorrentEvent` broadcast channel for piece completion, peer connect, or state transitions.
- Typical frontend consumes via HTTP polling of `/torrents/{id}/stats/v1`.

## Stats persistence

**Not persisted across restarts.**

- `SessionStats` (`session_stats/mod.rs:22-28`) holds in-memory atomics + `startup_time: Instant`. No flush to disk.
- `SerializedTorrent` (`session_persistence/json.rs:24-27`) persists only `{info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused}` — **no upload/download counters**.
- Bitfield resume data **is** persisted: `.bitv` files via `store_initial_check()` (`session_persistence/json.rs:213-239`).

**Magpie implication**: consumer owns cumulative stats (lightorrent already does this in redb). Magpie persists only protocol-level resume state.

## Public-ness audit

Key consumer-facing types are `pub` in the current tree:

| Type | Pub? | Cite |
|---|---|---|
| `ManagedTorrentHandle` (`= Arc<ManagedTorrent>`) | pub | `torrent_state/mod.rs:616` |
| `TorrentStats` | pub, serializable | `torrent_state/mod.rs:70` |
| `ManagedTorrentState` enum | pub | `torrent_state/mod.rs:67` |
| `TorrentStatsState` | pub | `torrent_state/stats.rs:46` |
| `AddTorrentOptions` | pub, all fields pub | `session.rs:243` |
| `Session` | pub struct | `session.rs:107` |

Internal state structs (`TorrentStateInitializing` etc.) are `pub` but with `pub(crate)` fields — reasonable.

**Historical PRD claim** of widespread `pub(crate)` appears to reflect librqbit 8; current version has opened much of the surface.

## Session state file

- Location: `output_folder/session.json` (`session_persistence/json.rs:44`).
- Shape: `SerializedSessionDatabase { torrents: HashMap<usize, SerializedTorrent> }` — metadata only.
- Per-torrent files: `{info_hash}.torrent` (cached bytes) and `{info_hash}.bitv` (resume bitfield).
- **Does NOT persist**: upload_bytes, downloaded_bytes, seeding duration, consumer stats.

**Magpie decision confirmed**: write only bitfield + minimal metadata for resume. Consumer stats live in consumer's store.

## Peer-ID construction

Hardcoded in `session.rs:517-519`:
```rust
let peer_id = opts.peer_id.unwrap_or_else(|| generate_azereus_style(*b"rQ", crate_version!()));
```

- Prefix `*b"rQ"` is baked in.
- `SessionOptions::peer_id: Option<Id20>` (`session.rs:420`) lets you pass a fully pre-built 20-byte ID, but skips the randomized suffix generator.

**Gap confirmed.** Magpie needs `PeerIdBuilder { client_code: [u8; 2], version: [u8; 4] }` → client-controlled prefix + deterministic-randomized suffix.

## librqbit-utp

No separate `librqbit-utp` crate in this tree. Transport abstraction (`stream_connect.rs`, `StreamConnector`) handles TCP + SOCKS, not uTP. librqbit relies on TCP + NAT traversal (STUN/UPnP).

**Magpie implication**: the "borrow uTP design from librqbit-utp" line in PROJECT.md's inspiration table is **stale**. Our uTP (M4) will have to build on different references: rakshasa, rasterbar, or a fresh design. Record this in ADR 0001 and revise the inspiration table.

## Architectural regrets — `Arc<Mutex<Session>>` god-object claim

**Refuted in current code.**

- Session wrapped in `Arc<Session>`, not `Arc<Mutex<Session>>`.
- State uses `RwLock<SessionDatabase>` for the torrent map + atomic counters for hot fields (`session.rs:109`).
- Torrents independently owned as `Arc<ManagedTorrent>`, each with its own small `RwLock<ManagedTorrentLocked>` for state transitions.

The architecture is fine-grained. The PRD's claim reflects an older version; the gap should be restated as "borrowing cross-cutting state through the session type at all" rather than "monolithic mutex".

## What magpie should borrow

1. **`Arc<Session>` + per-torrent `Arc<ManagedTorrent>`** ownership shape. Clean, async-friendly.
2. **State machine for torrent lifecycle** (`Initializing → Paused ↔ Live → Error`). Clear, correct, worth adopting almost verbatim.
3. **Typed `TorrentStats` struct** as the progress report payload. Extend with cumulative counters.
4. **Persistence abstraction trait** (`SessionPersistenceStore`) — magpie's resume-data backend should be pluggable similarly.
5. **CancellationToken + DropGuard** for cascaded shutdown across all spawned tasks.

## What magpie should avoid

1. **On-demand stats computation** without any event stream. Add `broadcast::<TorrentEvent>` from M0; emit `PieceCompleted`, `StateChanged`, `PeerAdded`, `StatsTick`.
2. **Session file duplicating consumer state.** Never.
3. **Hardcoded peer-ID prefix.** Parametrise via builder.
4. **No persistent event log / no late-subscriber catchup.** Consumers subscribing after startup should still see current state via a snapshot API + future events.

## ADR seeds

- **ADR 0002 (event bus)**: librqbit's polling confirms the core gap magpie is solving. `broadcast<TorrentEvent>` is the right shape.
- **Inspiration-table correction**: remove "Userspace uTP + metrics | librqbit-utp" line from PROJECT.md — that crate doesn't exist in current librqbit.
