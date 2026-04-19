# M3 — Extension protocol + Magnet + PEX + LSD

**Status**: in-progress
**Gate summary**: `magnet:?xt=...&tr=...` add works end-to-end (metadata fetched from peers, download completes, SHA-256 match); PEX discovers at least one additional peer in a controlled-swarm scenario; LSD announces are sent and received on loopback.

## Goal

Make magpie usable with magnet links — the dominant way users add torrents in practice. Implement the BEP 10 extension protocol infrastructure that all future peer-level extensions (PEX, `ut_metadata`, `ut_holepunch`, etc.) depend on, then build `ut_metadata` (BEP 9) on top for magnet support. PEX (BEP 11) and LSD (BEP 14) ride the same milestone because they're small, self-contained, and improve peer discovery without requiring DHT (which is its own milestone — M4).

## Scope / deliverables

### A. Extension protocol infrastructure (BEP 10)

- [x] Parse the extension-handshake message (bencode `m` dict mapping extension names to IDs): `crates/magpie-bt-wire/src/extension.rs`. Handles `m`, `metadata_size`, `p`, `v`, `yourip`, `reqq`. Rejects ID 0 (BEP 10: disabled), non-UTF8 names, >128 extensions, metadata_size >16 MB.
- [x] `ExtensionRegistry` — per-peer negotiated ID mapping: `crates/magpie-bt-wire/src/extension.rs`. `with_local()`, `set_remote()`, `remote_id()`, `local_id()`, `our_handshake()`.
- [x] Wire extension-handshake exchange into `PeerConn` post-handshake: `peer.rs::exchange_extension_handshake()`. Sends ours, waits (configurable timeout) for theirs, populates registry. Lenient: non-handshake first messages processed normally; timeouts don't kill the connection.
- [x] Dispatch `Message::Extended` payloads to the appropriate handler by extension ID: `peer.rs::handle_inbound()`. Uses `ExtensionRegistry::local_name_for_id()` for reverse lookup. Forwards as `PeerToSession::ExtensionMessage`. Late handshakes (id=0) update the registry. Payload size capped at 1 MB.
- [x] Fuzz target for extension-handshake parser + corpus: `crates/magpie-bt-wire/fuzz/fuzz_targets/extension_handshake.rs`, 4 seed corpus entries.

### B. Metadata exchange — `ut_metadata` (BEP 9)

- [x] `MetadataMessage` codec: `request` / `data` / `reject` message types in `crates/magpie-bt-wire/src/metadata.rs`. Handles the BEP 9 trailing-data quirk. Per-piece size bounded to 16 KiB. `metadata_piece_count()` helper.
- [x] Session-level metadata assembler: `crates/magpie-bt-core/src/session/metadata_exchange.rs`. Piece-wise reassembly, SHA-1 verification, parse into TorrentParams. Bounds: MAX_METADATA_SIZE 16 MB, piece_count ≤2M, piece_length ≤64 MB. Max 3 verification failures before giving up.
- [x] Advertise `metadata_size` in our extension-handshake when we have the metadata (seeder side): plumbed via `PeerConfig::metadata_size`.
- [x] Serve metadata requests from peers: `torrent.rs::serve_metadata_piece()`. Bounds-checked piece index.
- [x] Request metadata from peers when starting from a magnet link: `torrent.rs::request_metadata_from_peer()` / `request_metadata_from_any_peer()`.
- [x] Session integration: `Engine::add_magnet()` with `AddMagnetRequest`. Creates metadata-fetching session, emits `Alert::MetadataReceived` on success, auto-transitions to downloading.

### C. Magnet URI parser

- [x] Parse `magnet:?xt=urn:btih:<hex-or-base32>&dn=<name>&tr=<tracker>&...`: `crates/magpie-bt-metainfo/src/magnet.rs`. Supports hex and base32 info hashes, URL percent-encoding, duplicate `xt` rejection, parameter size limits (4096 bytes), collection limits (100 trackers, 200 peers).
- [x] `MagnetLink` struct: `info_hash: [u8; 20]`, `display_name: Option<String>`, `trackers: Vec<String>`, `peer_addrs: Vec<SocketAddr>`. `Display` impl for round-trip serialization.
- [x] Location: `crates/magpie-bt-metainfo/src/magnet.rs`, exported via `lib.rs`.

### D. Peer Exchange — `ut_pex` (BEP 11)

- [x] PEX message codec: `crates/magpie-bt-wire/src/pex.rs`. `PexMessage` with `added: Vec<PexPeer>` + `dropped: Vec<SocketAddr>`. `PexFlags` newtype. Compact IPv4/IPv6 encode/decode. Flags alignment validated. MAX_PEX_PEERS=200 enforced.
- [x] Inbound PEX: `torrent.rs::handle_pex_inbound()`. Peer-cap validated, 10s per-peer rate limiting, discovered peers buffered in `pex_discovered` for engine polling via `drain_pex_discovered()`.
- [x] Outbound PEX: `torrent.rs::send_pex_round()` on 60s interval. Diff-based (rakshasa-style), max 50 added + 50 dropped per message. Private flag enforced.
- [x] Rate-limit: 60s outbound per peer (`PexState::should_send_to`), 10s inbound per peer (`PexState::should_accept_from`).
- [x] Reachable-address advertising: BEP 10 `p` field (our listen port) plumbed through `PeerConfig::local_listen_port` into the outbound extension handshake; inbound peers' `addr` is rewritten to `(remote_ip, their_listen_port)` so PEX rounds advertise dialable addresses, not ephemeral source ports.
- [x] Engine accessor: `Engine::drain_pex_discovered(torrent_id)` (proxied via `SessionCommand::DrainPexDiscovered`) returns the buffered PEX-discovered peer addresses for a torrent so consumers can feed them into `add_peer`.
- [x] 3-engine integration test: `tests/pex_discovery.rs` — A seeds, B + C `add_peer` A only, both leechers discover each other via PEX, complete the download, SHA-256 verified.

### E. Local Service Discovery — LSD (BEP 14)

- [x] Multicast announce on `239.192.152.143:6771` (IPv4) with `BT-SEARCH * HTTP/1.1` + `Infohash:` header: `crates/magpie-bt-core/src/lsd.rs`. `LsdAnnounce` codec with encode/decode, lenient line-ending parsing. SO_REUSEADDR + multicast TTL=1 via `socket2`.
- [x] Listener: `LsdService` actor — joins multicast group, receives announcements, reports discoveries via `mpsc::Sender<LsdDiscovery>`. Cookie-based self-filtering. CancellationToken shutdown.
- [x] Respect the private flag: no LSD on private torrents — `register()` silently skips private hashes.
- [x] Guard: `LsdConfig { enabled, announce_interval, multicast_addr }` — configurable enable/disable.

## Gate criteria (verification)

1. **All DISCIPLINES bars hold workspace-wide**: `cargo test`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace --no-deps -D warnings` clean.
2. **Magnet end-to-end (hard gate)**: integration test — magpie-seed holds a synthetic torrent + serves metadata via `ut_metadata`; magpie-leech starts from a `magnet:?xt=...&tr=...` URI, fetches metadata from the seed, downloads all pieces, SHA-256 match. No `.torrent` file given to the leecher.
3. **Magnet interop**: magpie-seed with `ut_metadata` → qBittorrent or Transmission leecher starts from the same magnet URI, downloads successfully. Best-effort for M3 (promoted to hard gate in M4 if it slips). Wired as new compose scenarios `qbittorrent-magnet` / `transmission-magnet` (`ci/interop/docker-compose.{client}-magnet.yml`) with companion gate scripts (`gate_{client}_magnet.sh`). Seeder runs with `--advertise-metadata` so its BEP 10 extension handshake exposes `metadata_size` and serves `ut_metadata` Data; `generate_fixture` writes `fixture.magnet` (`magnet:?xt=urn:btih:<hex>&dn=...&tr=...`) which the gate script feeds to the leecher's add-via-URL endpoint. Run via `ci/interop/run.sh qbittorrent-magnet` / `transmission-magnet`.
4. **PEX discovery**: controlled-swarm test with 3 magpie engines — A seeds, B leeches from A (via direct `add_peer`), C leeches from A (via direct `add_peer`). B and C discover each other via PEX without being directly connected. Verify C receives at least one piece from B after PEX discovery.
5. **LSD announce + receive**: unit/integration test — two engines on loopback, one announces via LSD, the other receives and adds the peer. Private torrents must NOT announce.
6. **Extension handshake fuzz**: fuzz target for extension-handshake parser, corpus committed, nightly CI ≥10 min.
7. **BDD coverage**: `.feature` files for BEP 9, 10, 11, 14 under `crates/magpie-bt/tests/features/`; `bep-coverage.md` rows updated.
8. **ADRs landed** for any non-trivial design decisions (extension registry shape, metadata exchange flow, PEX rate limiting).

## ADRs expected

- **ADR-0021** Extension registry: per-peer negotiated ID map shape, dispatch mechanism, how new extensions register.
- **ADR-0022** Metadata exchange flow: piece-size, retry policy, max-concurrent-requests, hash verification ordering, transition from "metadata-fetching" to "downloading" state.
- Others as needed during implementation.

## Open questions

- **LSD on macOS**: `IP_ADD_MEMBERSHIP` on macOS may require binding to `INADDR_ANY` rather than the multicast group address. Verify during implementation; if problematic, gate LSD behind `#[cfg(not(target_os = "macos"))]` or a runtime flag.
- **Extension handshake `yourip`**: BEP 10 includes a `yourip` field for NAT detection. Worth implementing in M3 or defer to M6 (UPnP/NAT-PMP)?

## Out of scope

- DHT (BEP 5) → M4. Magnet links work with tracker URLs in M3; trackerless magnet support requires DHT.
- v2 magnet URIs (`btmh` / multihash) → M5 (BEP 52).
- uTP (BEP 29) → M5.
- `ut_holepunch` (BEP 55) → M4 or later (depends on DHT + uTP).
- Extension protocol encryption / obfuscation (BEP 6 already covers fast extension; MSE/PE is out of scope).
