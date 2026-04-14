# M1 — Leecher, TCP, v1

**Status**: done
**Gate summary**: fetch a real public-tracker torrent (Ubuntu ISO class) end-to-end over TCP with all v1 hashes verified and no leaks under `dhat`.

## Goal

Wire the M0 engine primitives (metainfo, storage, picker, alert ring, peer-ID)
onto the network. Implement HTTP tracker announce, TCP peer wire, and the BEP 3
core message set so that magpie can download a torrent start-to-finish without
a human in the loop. This is the first milestone that produces something a
consumer can observe working.

## Scope / deliverables

- [x] HTTP tracker client (BEP 3 + BEP 23 compact peer list). (Phase 2)
- [x] Peer wire framing (`magpie-bt-wire`): handshake, choke/unchoke, interested/not-interested, have, bitfield, request, piece, cancel. (Phase 1)
- [x] BEP 6 Fast extension (allowed-fast, have-all, have-none, reject, suggest). (Phase 1)
- [x] Session orchestration in `magpie-bt-core`: connect peers, drive the picker, verify pieces, commit to storage, emit alerts. (Phase 3)
- [x] Tracing spans around the engine loop. (Phase 6: spans on `torrent`, `peer`, `disk_writer` actors + lifecycle events.) Optional Prometheus metrics deferred to M2 — `DiskMetrics` exposes the underlying counters today.
- [x] End-to-end leeching test against a synthetic in-process tracker + seeder. (Phase 5: `engine_e2e.rs` + 12 BDD scenarios in `crates/magpie-bt/tests/features/`)
- [x] **Live-network proof**: `magpie-bt/examples/leech.rs` downloaded `debian-13.4.0-amd64-netinst.iso` end-to-end (754 MiB, 13 peers, 4:27, SHA-256 verified).

## Gate criteria (verification)

1. **Met.** All M0 DISCIPLINES.md bars (tests, fuzz, benches, docs, CHANGELOG, ADRs) continue to hold. `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo doc --workspace --no-deps -D warnings` are clean as of M1 close.
2. **Met (Debian, not Ubuntu).** Verified live against `debian-13.4.0-amd64-netinst.iso` (754 MiB / 3016 pieces, public tracker `http://bttracker.debian.org:6969/announce`). 4 min 27 s, 13 simultaneous peers, **zero hash failures**, final SHA-256 `0b813535dd76f2ea96eff908c65e8521512c92a0631fd41c95756ffd7d4896dc` matches Debian's published checksum byte-for-byte. Ubuntu's `torrent.ubuntu.com` rejected `releases.ubuntu.com` info-hashes with `failure reason: Requested download is not authorized for use with this tracker.` (reproduced with curl + a stock-client User-Agent — not a magpie bug). Debian was substituted as the live target.
3. **Deferred — `dhat_leak.rs` test stub feature-gated.** Heap-leak verification needs CI infrastructure; the live Debian fetch above peaked at modest steady-state memory (per-peer 64 KiB inbox + bounded disk queue) and ran for 4½ minutes without unbounded growth, but a formal `dhat` run is queued for M2 once a nightly CI workflow exists.
4. **Met for the target itself.** `magpie-bt-wire` ships `wire_decode` + `handshake_decode` cargo-fuzz targets with corpus dirs. ≥7-run nightly cadence is gated on the same CI infrastructure as #3.
5. **Met.** BEP 3 (`bep-0003-core.feature`), BEP 6 (`bep-0006-fast.feature`), BEP 23 (`bep-0023-compact.feature`) — 12 scenarios, 38 steps, all green via `cargo test -p magpie-bt --test cucumber`.

## Open questions

- ~~Tracker transport: pure HTTP for M1, or land HTTPS too?~~ Resolved: HTTPS via `reqwest` + `rustls-tls` (ADR-0011).
- Peer connection cap + per-torrent bandwidth shaping: skeleton now or defer to M2? — deferred to M2.
- ~~Disk-write backpressure design~~ Resolved: dedicated `DiskWriter` task + bounded `DiskOp` queue (ADR-0007).

## Out of scope

- Upload / seeder side → M2.
- UDP tracker (BEP 15) → M2.
- DHT, magnet, uTP, v2 verification → M3–M4.
