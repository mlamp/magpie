# magpie-bt-core

Engine core for the [magpie](https://github.com/mlamp/magpie) BitTorrent library: piece picker, storage trait, alert ring, peer-ID builder.

**Status**: M0 phase C complete — peer-ID builder, custom alert ring (ADR-0002), `Storage` trait with memory + file impls, rarest-first piece picker with endgame.

## Modules

- [`peer_id`] — 20-byte Azureus-style peer-ID generation with OS-backed randomness.
- [`alerts`] — custom double-buffered alert ring (ADR-0002): category masks, overflow sentinel, async `wait()`.
- [`storage`] — `Storage` trait with `MemoryStorage` (all platforms) and `FileStorage` (Unix only for M0). File storage currently uses `FileExt::{read_at,write_at}` (no unsafe). Vectorised `preadv`/`pwritev` is on the backlog behind ADR-0008.
- [`picker`] — piece picker skeleton: rarest-first with per-piece availability tracking and endgame-mode switching.

This crate is on the `unsafe` allowlist in [`DISCIPLINES.md`](https://github.com/mlamp/magpie/blob/main/docs/DISCIPLINES.md), reserved for the vectorised-I/O path in a follow-up. As of Phase C, the crate is `unsafe`-free.
