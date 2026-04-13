# Librqbit gap analysis (historical)

> **Archived**: this is the original motivation analysis from the pre-M0 project definition. It documents the state of `librqbit 8` as lightorrent experienced it in 2026-04. It is preserved for *why we started magpie* context only. Current motivation in [../PROJECT.md](../PROJECT.md); in-flight scope in [../MILESTONES.md](../MILESTONES.md).

## Gaps in librqbit that forced magpie

Lightorrent ran on `librqbit 8`. It worked, but with gaps that produced workarounds:

- No persistent upload/download stats (counters reset on restart/pause).
- No event/messaging system for piece-level activity — we polled instead.
- Useful types were `pub(crate)` and could not be named in consumer code.
- Session state file duplicated what we track in redb.
- Peer-ID prefix was hardcoded to `-rQ????-`, unusable for private-tracker client whitelisting.

## Gap → Requirement mapping

| Librqbit gap | Magpie requirement |
|---|---|
| No event bus | Typed `TorrentEvent` on `tokio::sync::broadcast`; bounded, slow consumers get `Lagged`, never backpressure the engine. |
| Stats not persisted; consumers poll | Event-driven cumulative stats; consumers accumulate via subscription. |
| Types `pub(crate)` | Every type needed to drive the API is `pub`: torrent handle, stats, add-options, state enum. |
| Session file / redb duplication | Magpie persists only protocol-level state (resume data, piece bitfield). Consumer state (stats, ratios, history) lives in the consumer's store. |
| Hardcoded peer-ID | Peer-ID builder takes `client_code: [u8; 2]`, `version: [u8; 4]`, random suffix. |

## Why not fork librqbit

- Fork inherits the `Arc<Mutex<Session>>` god-object that motivated a fresh API.
- Public surface had grown organically; redesign from lightorrent's call sites was cheaper than refactoring in place.
- v2/hybrid data model wanted abstraction at the hash layer that librqbit did not have.
