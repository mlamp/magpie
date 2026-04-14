## [0.1.0-alpha.2] - 2026-04-14

### 🚀 Features

- *(soak)* Add run-24h.sh for server-side full soak run
- *(alerts)* Add TorrentId to torrent-scoped alert variants
## [0.1.0-alpha.1] - 2026-04-14

### 🚀 Features

- *(M0)* Initialise Cargo workspace + BDD + fuzz + bench scaffolds
- *(M2)* Seeder, multi-torrent, interop harness, dhat soak, consumer audit

### 🐛 Bug Fixes

- *(ci)* Rustfmt, rustdoc, cargo-deny, test timeouts
- *(ci)* Clippy too_many_lines, rustdoc links, test deadline
- *(ci)* Bump e2e test deadlines to 60s for CI runners
- *(ci)* Serialize tests to prevent resource starvation on shared runners
- *(ci)* Use RUST_TEST_THREADS env var, bump all e2e deadlines
- *(interop)* QBittorrent password + Transmission gate robustness
- *(ci)* Reduce mock tracker interval + diagnostic on attach_tracker
- *(interop)* QBittorrent pre-configured config + Transmission 409 retry
- *(interop+soak)* Qbt 4.6.7 pin, transmission curl fix, soak resilience
- *(ci)* Soak handles.capacity() borrow-after-move + interop debug
- *(interop)* Qbt 4.5.5 pin + initial container log dump
- *(interop)* Seeder --data path must be fixture.bin not interop.bin

### 💼 Other

- Read reference implementations; update ADRs
- Rework 0002 as custom rasterbar-style alert ring
- Accept 0001 (subcrates), 0002 (alert ring), 0003 (tokio-only)
- *(ci)* Add diagnostic alert dump to failing e2e tests
- *(interop)* More Transmission session-ID diagnostics

### ⚙️ Miscellaneous Tasks

- Add debian trixie test job + RUST_LOG debug on all linux tests
