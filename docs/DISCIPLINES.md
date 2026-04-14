# magpie — Disciplines

The bars every change must clear. Scan in ≤2 minutes. Anything that isn't enforced mechanically by CI is *not* a discipline — it's a hope.

## Safety

- `#![forbid(unsafe_code)]` at every crate root **except** the documented allowlist.
- **Unsafe allowlist**: `magpie-bt-core` (file I/O syscalls), `magpie-bt-utp` (socket buffer handling, when it lands). Every `unsafe` block must have a `// SAFETY:` comment stating the invariants that make it sound.
- Lints on every crate root:
  ```rust
  #![warn(missing_docs)]
  #![warn(clippy::pedantic, clippy::nursery)]
  #![deny(clippy::undocumented_unsafe_blocks)]
  ```
- `unsafe` introductions require an ADR.

## Testing

| Kind | Where | Mandatory for |
|---|---|---|
| Unit + integration | `tests/` in the same crate/PR | every module |
| Property (`proptest`) | `proptest/` or `#[cfg(test)] mod proptest` | `-bencode`, `-metainfo`, piece picker |
| Fuzz (`cargo-fuzz`) | `fuzz/fuzz_targets/` | parsers from M0 (`bencode`, `metainfo`); wire codec from M1 |
| Benchmarks (`criterion`) | `benches/` | hot paths: picker, bencode decode, storage write |
| BDD (`cucumber-rs`) | `crates/magpie-bt/tests/features/`, `crates/magpie-bt/tests/steps/` | every BEP in flight; one `.feature` file per BEP; tracked in [`bep-coverage.md`](bep-coverage.md) |
| Interop (real clients) | `tests/interop/` | from **M2** onward (milestone gate) |

**Coverage thresholds** (`cargo-llvm-cov`):
- `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`: ≥90 % lines.
- `magpie-bt-core`: ≥80 % lines.
- Overall project: ≥85 % lines.

**Fuzz cadence**: nightly CI, ≥10 min per target, corpus committed under `fuzz/corpus/<target>/`.

**Benchmark regressions**: criterion baseline committed; ≥5 % regression on a tracked benchmark fails CI.

## Engineering

- **MSRV**: stable − 2 minor versions. Recorded in `clippy.toml` and each `Cargo.toml` `rust-version`. Bumps require a `CHANGELOG.md` entry.
- **CI** (`.github/workflows/ci.yml`): `fmt`, `clippy -D warnings`, `test` on Linux + macOS, `doc` (`RUSTDOCFLAGS=-D warnings`), `cargo-deny check`, `cargo-llvm-cov` with thresholds.
- **Nightly** (`.github/workflows/nightly.yml`): `cargo-fuzz` per target + `cargo miri test --lib`.
- **Supply chain** (`deny.toml`): GPL-family denied; RUSTSEC advisories deny-by-default (exceptions require an ADR); duplicate versions warn.
- **SemVer**: `cargo-public-api` diff posted on every PR once a crate reaches v0.1. Pre-0.1 is best-effort but breaking changes still land in the CHANGELOG.
- **Docs**: every `pub` item has a rustdoc summary line. Every crate root has an intro + runnable example. CI: `cargo doc --no-deps -D warnings`.

## Process

- **ADRs** (`docs/adr/NNNN-title.md`, Michael Nygard format) for non-trivial design decisions. Index in `docs/adr/README.md`. An ADR is required before introducing `unsafe`, adding a dependency with a non-standard license, or taking a hard runtime/ecosystem commitment.
- **CHANGELOG.md** updated on every PR that affects public API, behaviour, or MSRV.
- **Milestone files** are the source of truth for in-flight scope. New milestones use `docs/milestones/_template.md`.
- **Public API is client-agnostic; lightorrent's call sites are a completeness check, not the shape driver.** If a realistic BitTorrent client would need to reach into internals, that is an API bug in magpie.

## Security

- Reporting channel: GitHub private security advisories on `mlamp/magpie` (see `SECURITY.md`).
- Parsers treat every input as hostile. Panics in parser code paths are security bugs, not crashes.
- Allocation bounds: every parser that sizes an allocation from attacker input must clamp that size to a documented per-message ceiling.

## What is *not* a discipline

Best practices that aren't enforced by CI live in `CONTRIBUTING.md` or milestone files. They are advisory. Don't add a rule here unless CI (or a gate script) can check it.
