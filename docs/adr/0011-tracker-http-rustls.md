# 0011 — Tracker HTTP transport: `reqwest` + `rustls-tls`

- **Status**: accepted
- **Date**: 2026-04-13
- **Deciders**: magpie maintainers
- **Consulted**: M1 plan review (lightorrent dogfood)

## Context

M1 ships an HTTP tracker client (BEP 3 + BEP 23). The Ubuntu ISO end-to-end
gate (`docs/milestones/001-leecher-tcp-v1.md` §"Gate criteria" #2) requires
real-world tracker connectivity, and Ubuntu's tracker
(`https://torrent.ubuntu.com/announce`) is HTTPS-only. We therefore need TLS
in M1 — the question is which TLS backend.

`reqwest` offers two paths:

1. `rustls-tls` — pure-Rust TLS, depends on `ring` / `aws-lc-rs`. No system
   library required.
2. `native-tls` — Schannel on Windows, SecureTransport on macOS, OpenSSL on
   Linux (the Linux builder must have `libssl-dev`).

## Decision

magpie uses `reqwest = { default-features = false, features = ["rustls-tls",
"http2"] }` for the M1 HTTP tracker. The same TLS backend is the default for
any future HTTP-shaped subsystem (e.g. WebSeed in M5).

## Consequences

Positive:

- Identical builds across Linux/macOS/Windows; no `libssl-dev` install step on
  CI runners or downstream consumer machines.
- Pure-Rust dependency tree aligns with magpie's `forbid(unsafe_code)` /
  documented-allowlist posture (only `ring` / `aws-lc-rs` carry `unsafe`, and
  both are widely audited).
- Reproducible across cross-compilation targets (no system OpenSSL ABI to
  match).

Negative:

- AES-NI hardware acceleration is slightly behind OpenSSL on x86-64, but
  tracker traffic is one TLS handshake per re-announce interval (default
  ≥1800 s), so the gap is irrelevant in practice.
- ~15 transitive dependencies pulled in (`ring`, `rustls`, `rustls-pki-types`,
  `webpki-roots`, etc.). All under permissive licenses (`deny.toml` review
  performed).

Neutral:

- The `Tracker` trait keeps the transport pluggable, so a consumer that needs
  `native-tls` (e.g. for OS root-trust integration) can implement `Tracker`
  with their own `reqwest::Client`. Magpie itself ships `rustls-tls` only.

## Alternatives considered

- `native-tls`: rejected for cross-platform CI cost and `libssl-dev`
  installation friction. Would have shaved ~10 transitive deps but added one
  external system requirement we don't otherwise need.
- HTTP-only (no TLS) in M1: rejected — would require swapping the live-fetch
  gate to a plain-HTTP tracker (e.g. archive.org), weakening the gate's
  representativeness for real-world public-tracker fetches.
- Custom HTTP client (e.g. `hyper` directly): rejected as scope creep.
  `reqwest` is mature, well-maintained, and matches librqbit's choice — no
  reason to roll our own.
