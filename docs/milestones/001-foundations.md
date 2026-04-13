# M0 — Foundations (no network)

**Status**: in-progress
**Gate summary**: parse real v1/v2/hybrid .torrents; storage round-trips blocks; picker sane on synthetic bitfields; fuzz targets green for bencode + metainfo; coverage at DISCIPLINES bars.

## Goal

Stand up the non-network spine of magpie: workspace layout, on-disk data structures, and the internal plumbing (event bus, picker, storage trait) that every later milestone will reuse. No sockets, no trackers, no peer wire yet. Establish every quality bar in [`../DISCIPLINES.md`](../DISCIPLINES.md) so M1+ starts on solid ground.

## Scope / deliverables

### Workspace & crates

- [x] Cargo workspace at repo root with member crates:
  - [x] `magpie-bt-bencode` — placeholder; zero-copy bencode encode/decode lands next.
  - [x] `magpie-bt-metainfo` — placeholder; .torrent parsing for v1, v2, hybrid lands next.
  - [x] `magpie-bt-wire` — placeholder; peer wire protocol codec lands in M1.
  - [x] `magpie-bt-core` — placeholder; `Storage` trait, picker, alert ring land next.
  - [x] `magpie-bt` — facade crate with cucumber BDD harness scaffolded.
- [x] Every crate: `Cargo.toml` with SPDX `license = "Apache-2.0 OR MIT"` and matching `rust-version`.
- [x] Every crate root applies the lint block from DISCIPLINES.md (`forbid(unsafe_code)` everywhere except `magpie-bt-core` which uses `deny(unsafe_code)` per the allowlist).

### Functional deliverables

- [ ] `Storage` trait + file-backed impl with vectorised `pwritev`/`preadv` on Unix (behind `unsafe` allowlist); in-memory impl for tests.
- [ ] Piece picker skeleton: rarest-first + endgame mode, no network wiring.
- [ ] Event bus: `tokio::sync::broadcast<TorrentEvent>` with bounded channel semantics; `Lagged` never backpressures.
- [ ] Azureus-style peer-ID builder: `client_code: [u8; 2]`, `version: [u8; 4]`, random suffix.
- [ ] v2 invariant enforcement in v1 paths: 16 KiB blocks, power-of-two piece sizes.
- [ ] Typed errors per module using `thiserror`.

### Quality deliverables (per DISCIPLINES.md)

- [ ] **Property tests** (`proptest`) for `-bencode` round-trip, `-metainfo` parse/re-encode invariants, picker on synthetic bitfields.
- [ ] **Fuzz targets** (`cargo-fuzz`): `bencode_decode`, `metainfo_parse`. Seed corpora committed; `fuzz/` directory wired into the `nightly.yml` workflow matrix.
- [x] **Benchmarks** (`criterion`): skeleton targets in place (`bencode/benches/decode.rs`, `metainfo/benches/parse.rs`, `core/benches/{picker,alert_ring}.rs`). Real bench bodies + baselines land with implementation.
- [x] **BDD** (`cucumber-rs`) harness scaffolded in `magpie-bt/tests/`; seed scenario in `features/bep-0003-core.feature` passes.
- [x] **BEP coverage matrix** at [`docs/bep-coverage.md`](../bep-coverage.md) populated with planned BEPs.
- [ ] **Coverage** thresholds met per DISCIPLINES.md (`-bencode`/`-metainfo` ≥90 %, `-core` ≥80 %, overall ≥85 %).
- [ ] **Docs**: every `pub` item has a rustdoc summary; each crate root has an intro + runnable example; `cargo doc -D warnings` passes.
- [ ] **CHANGELOG** under `## [Unreleased]` lists every public-API addition.

## Gate criteria (verification)

1. **Metainfo**: parse a corpus of real .torrent files covering v1-only, v2-only, and hybrid. All three produce the correct `InfoHash` variant. Fixtures live in `magpie-bt-metainfo/tests/fixtures/` (see "Test-fixture policy" below).
2. **Storage**: write N random blocks via file-backed `Storage`, read them back, assert byte-equality. Repeat on in-memory impl.
3. **Picker**: given synthetic swarm bitfields (uniform / skewed / near-complete), the picker selects pieces consistent with rarest-first; endgame kicks in at the expected threshold.
4. **Peer-ID builder**: produces 20-byte IDs matching the `-CCVVVV-` layout; two consecutive calls differ in the suffix.
5. **Fuzz targets** run nightly for ≥10 minutes each with zero crashes in the last 7 nightly runs before the gate.
6. **CI green** on every DISCIPLINES bar: `fmt`, `clippy -D warnings`, `test`, `doc -D warnings`, `cargo-deny check`, coverage ≥ thresholds.
7. **No `todo!()` / `unimplemented!()`** left in shipped code paths.

## Open questions

- [ADR-0001](../adr/0001-subcrate-vs-feature-dht-utp.md) — subcrate vs. feature for `dht`/`utp`. Resolve **before M0 close** so M3/M4 start on solid ground.
- [ADR-0002](../adr/0002-event-bus-alert-ring.md) — custom rasterbar-style alert ring (supersedes the earlier broadcast-based proposal). Deliver the ring under `magpie-bt-core/src/alerts/` during M0 with fuzzing + benchmarks.
- [ADR-0003](../adr/0003-tokio-only.md) — confirm at M0 kickoff.
- **Benchmark harness**: custom in-memory swarm in CI vs. real public torrents. Pick one during M0; scaffold under `magpie-bt-core/benches/`.
- **Test-fixture policy**: source, size ceiling, and license of the real .torrent corpus under `magpie-bt-metainfo/tests/fixtures/`. Prefer small public-domain content (e.g. Sintel, Big Buck Bunny torrents) + synthetic torrents generated in tests.
- **crates.io placeholder**: register `magpie-bt` (and the four workspace crate names) during M0 to prevent squatting.

## Out of scope

- Anything touching the network (trackers, peer wire, DHT, uTP). → M1+.
- Full BEP 52 merkle verification (data model only; actual verification lands in M4).
- Public API freeze — API will be shaped iteratively against lightorrent's call sites starting M2.
- Interop harness (lands at M2).
