# Interop harness

M2 gate criterion 5. Proves magpie interoperates with
real third-party BitTorrent clients (qBittorrent, Transmission) at
the handshake / wire / fast-ext level.

## Layout

- `Dockerfile.magpie` — multi-stage build producing three binaries
  (`magpie-seeder`, `mock_tracker`, `generate_fixture`).
- `docker-compose.qbittorrent.yml` — scenario: magpie seeds → qBittorrent leeches.
- `docker-compose.transmission.yml` — scenario: magpie seeds → Transmission leeches.
- `run.sh {qbittorrent|transmission}` — orchestrator.
- `gate_qbittorrent.sh` — per-client gate: adds torrent via qBittorrent
  webUI API, polls completion, SHA-256 verifies the download.
- `gate_transmission.sh` — per-client gate: adds torrent via Transmission
  RPC API, polls completion, SHA-256 verifies the download.

## Image versions

Third-party client images are pinned to specific version tags in the
compose files (`linuxserver/qbittorrent:5.0.4`,
`linuxserver/transmission:4.0.6`). Update these periodically via a
digest-bump PR.

## qBittorrent password

linuxserver/qbittorrent 4.6+ generates a random temporary admin
password on first boot and writes it to the container log. Before
running locally, discover it via
`docker compose -f ... logs leech | grep "temporary password"` and
export it as `QBT_PASS=<value>`. Once captured, pin it in the image's
bind-mounted `config/qBittorrent.conf` so subsequent runs are stable.

## Status

- magpie seeder binary + mock HTTP tracker + deterministic fixture
  generator all shipped as workspace examples.
- Compose files boot the four-service stack on an isolated bridge
  network with static IPs (so the mock tracker can hand the leech a
  direct seeder IP).
- Per-client gate scripts (`gate_qbittorrent.sh`, `gate_transmission.sh`)
  drive the full add-torrent + completion-poll + SHA-256 verification
  flow for each client.
- `run.sh` dispatches to the appropriate gate script; CI matrix covers
  both scenarios.

## Local smoke

```
ci/interop/run.sh qbittorrent
# in another terminal, to follow logs:
docker compose -f ci/interop/docker-compose.qbittorrent.yml logs -f seeder
```

## What the mock tracker does

Returns a single-peer compact peer list (`10.88.0.10:6881` for the
qBittorrent scenario, `10.89.0.10:6881` for Transmission) for any
`/announce` request. Doesn't track swarms or scrape. Drives one thing:
making the third-party leech discover the magpie seeder without a
real tracker service in the picture.

## CI

The interop workflow (`.github/workflows/interop.yml`) runs both
scenarios on PR, push to main, and a daily schedule. Logs are uploaded
as artifacts on failure.
