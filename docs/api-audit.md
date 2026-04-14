# Consumer-surface audit

**Status**: complete for M2 (2026-04-14).
**Purpose**: verify `magpie-bt` + `magpie-bt-core` public surface is *client-agnostic* and *complete* for realistic BitTorrent-client call sites, without baking consumer-specific concepts into the library.

## Method

Walked the facade crate's re-export list (`crates/magpie-bt/src/lib.rs`) and the public items of `magpie-bt-core` (`Engine`, `AddTorrentRequest`, `Alert*`, `Storage*`, `Tracker*`, `TorrentParams/State`, `PeerIdBuilder`, `Picker`). Cross-referenced against three real call-site corpora:

1. **lightorrent/src/engine.rs** — the current reference consumer's pub fns (add/pause/resume/delete/stats/shutdown/etc.).
2. **libtorrent-rasterbar** (conceptual) — `session`, `torrent_handle` (pause/resume/file-priority/save-resume-data).
3. **anacrolix/torrent** (conceptual) — `Client`, `Torrent` (AddTorrent/Drop/Seeding/Pieces).

Excluded: lightorrent-specific concepts (ratio targets, category strings, redb-backed stats sidecar, per-torrent JSON file metadata). Those are consumer concerns and must not leak into magpie's surface.

## Coverage — what magpie already exposes and is correctly shaped

| Call-site need | magpie surface | Note |
|---|---|---|
| Construct engine | `Engine::new(AlertQueue) + with_global_peer_cap(cap)` | Builder-ish, client-agnostic. ✅ |
| Add torrent (file / URL / bytes) | `Engine::add_torrent(AddTorrentRequest)` → `TorrentId` | Single request type covers the variants via its builder. ✅ |
| Attach trackers at runtime | `Engine::attach_tracker(id, AttachTrackerConfig)` | ✅ |
| Accept inbound peers | `Engine::listen(ListenConfig)` + `add_peer` + `add_peer_stream` | ✅ |
| Subscribe to events | `AlertQueue` + `Alert` + `AlertCategory` | ADR-0002 style ring; client-agnostic. ✅ |
| Persistent stats | `trait StatsSink` + `FileStatsSink` | Consumer can swap sink (e.g. redb) without magpie knowing. ✅ |
| Disk metrics | `Engine::disk_metrics(id) -> Arc<DiskMetrics>` | ✅ |
| Per-torrent shutdown | `Engine::shutdown(id)` | ✅ |
| Await full shutdown | `Engine::join()` | ✅ |
| Peer identity | `PeerIdBuilder` | Azureus-style, configurable. ✅ |
| Storage backend | `trait Storage` + `FileStorage` + `MemoryStorage` | Pluggable. ✅ |
| Private flag | `TorrentParams::is_private()` (via metainfo) | ✅ |

## Gaps — client-agnostic tasks to file

Three gaps surfaced. Each is phrased as a general BitTorrent-client need, not a lightorrent need.

### G1. Per-torrent pause / resume

**Need**: every realistic client (lightorrent, qBittorrent, Transmission, rasterbar example clients) supports pausing a torrent (stop sending/receiving, keep state) and resuming it. magpie's `Engine` currently exposes only `shutdown(id)` (terminal) — no pause.
**Shape**: `Engine::pause(id: TorrentId) -> Result<(), TorrentNotFoundError>` / `Engine::resume(id) -> …`. Internally: the per-torrent actor already has enough state to stop driving the choker/shaper and drop peers without tearing down the torrent; a `PauseMessage` on the actor's mpsc suffices.
**Not in M2 critical path** if no gate needs it — but **should land before the gate review** as part of closing G1–G3 together, because a consumer cannot realistically integrate without pause.
**Priority**: high. File as a task.

### G2. Remove torrent (with optional file deletion)

**Need**: clients distinguish "stop this torrent" from "stop and delete the downloaded files". Currently `Engine::shutdown(id)` stops the actor but does not remove data.
**Shape**: `Engine::remove(id: TorrentId, delete_files: bool) -> Result<(), TorrentNotFoundError>`. The `delete_files: bool` boolean argument is a client-agnostic shape (`bool` newtype is overkill; consider an enum `RemovalMode::{KeepFiles, DeleteFiles}` if the API surface review wants stronger typing, but `bool` follows rasterbar / anacrolix convention).
**Priority**: high. File as a task.

### G3. Enumerate live torrents

**Need**: a consumer that crash-restarts and wants to reconcile its persistent state with magpie's live state currently has no way to ask magpie "which torrents do you have loaded?". Forces every consumer to mirror magpie's registry, which is the leak PROJECT.md §Motivation was built to avoid ("Useful types are `pub(crate)` and can't be named in consumer code").
**Shape**: `Engine::torrents(&self) -> Vec<TorrentId>` (or an iterator) plus `Engine::torrent_state(id) -> Option<TorrentStateView>` (a read-only snapshot struct, not a handle).
**Priority**: high. File as a task.

## Deferred (out of M2 scope by plan; noted for future milestones)

- **Runtime reconfiguration of bandwidth caps** — construction-time only today. A `set_rate_limits(…)` on `Engine` that routes to the shaper (ADR-0013) is natural, but caps themselves are pass-through (`u64::MAX`) in M2 by design. Defer to M5 when cap-enablement actually matters. No gap today.
- **File-level priorities / streaming / priority pieces** — explicitly deferred to M6+ per ROADMAP.
- **DHT nodes count** — M3, not M2.
- **Save/restore resume data** — currently implicit via `FileStatsSink`; consumers wanting magnet-free rehydration will surface requirements in M3 with magnet work.

## Non-gaps — correctly *not* in the surface

These appear in lightorrent's engine but **must not** bleed into magpie:

- `set_ratio_target(hash, f64)` — ratio enforcement is a consumer policy layer built atop magpie's stats + pause. magpie exposes the primitives; the policy stays in the client.
- `set_category(hash, str)` — pure consumer metadata. magpie has no notion of "category" and should not acquire one.
- `get_files(hash) -> serde_json::Value` — JSON-shaped file metadata is a web-API concern of lightorrent. magpie exposes file structure via `metainfo::MetaInfo`; JSON shaping is the consumer's job.
- `cancel_token()` — consumers can wrap their own `CancellationToken` around `Engine::join()`. Exposing one from magpie would entangle it with a specific tokio-util version.
- `session()` / `registry()` — lightorrent internals; nothing to port.

## Follow-up tasks

- [ ] **G1** — implement `Engine::pause(id)` / `Engine::resume(id)`.
- [ ] **G2** — implement `Engine::remove(id, delete_files)`.
- [ ] **G3** — implement `Engine::torrents()` and `Engine::torrent_state(id)`.

All three are client-agnostic additions and should land in a single small PR before the M2 gate review. They are additive and do not change existing signatures.

## Verdict

magpie's public surface is **client-agnostic today** — no lightorrent-isms have leaked in — but has three completeness gaps (G1–G3) that any realistic consumer would hit. Filing as tasks and closing before gate review. Audit passes under the "would a BitTorrent client author unfamiliar with lightorrent find this natural?" test modulo those three additions.
