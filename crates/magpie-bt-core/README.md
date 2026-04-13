# magpie-bt-core

Engine core for the [magpie](https://github.com/mlamp/magpie) BitTorrent library: piece picker, storage trait, alert ring, session orchestration.

This crate is on the `unsafe` allowlist in [`DISCIPLINES.md`](https://github.com/mlamp/magpie/blob/main/docs/DISCIPLINES.md) — the storage backend uses `pwritev`/`preadv` on Unix. All other code in the crate is `unsafe`-free.

**Status**: pre-M0. Empty placeholder; real implementation lands during milestone [M0 — Foundations](https://github.com/mlamp/magpie/blob/main/docs/milestones/001-foundations.md).
