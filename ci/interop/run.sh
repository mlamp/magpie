#!/usr/bin/env bash
# Orchestrator for interop scenarios. Usage:
#
#   ci/interop/run.sh qbittorrent
#   ci/interop/run.sh transmission
#
# The per-scenario compose file boots tracker + magpie seeder + the
# third-party leech. This script:
#
#   1. builds the magpie-interop image (Dockerfile.magpie),
#   2. brings the scenario up,
#   3. adds fixture.torrent.with-announce to the leech via its HTTP API,
#   4. polls for download completion,
#   5. SHA-256s the leech's downloaded copy against the fixture,
#   6. tears down.
#
# Exit status: 0 on SHA-256 match within the deadline, non-zero otherwise.
#
# Per-client gate scripts (gate_qbittorrent.sh, gate_transmission.sh)
# drive the add-torrent + completion-poll + SHA-256 verification flow
# for each client's HTTP/RPC API. run.sh dispatches to the appropriate
# gate script and relays its exit status.
set -euo pipefail

SCENARIO="${1:-}"
case "$SCENARIO" in
  qbittorrent|transmission|qbittorrent-magnet|transmission-magnet) ;;
  *)
    echo "usage: $0 {qbittorrent|transmission|qbittorrent-magnet|transmission-magnet}" >&2
    exit 2
    ;;
esac

COMPOSE_FILE="$(dirname "$0")/docker-compose.${SCENARIO}.yml"
[ -f "$COMPOSE_FILE" ] || { echo "missing: $COMPOSE_FILE" >&2; exit 2; }

# `qbittorrent-magnet` → gate_qbittorrent_magnet.sh; rewrite `-` → `_` so
# the gate script naming convention matches the scenario id.
GATE_NAME="${SCENARIO//-/_}"

DEADLINE_SECS="${INTEROP_DEADLINE_SECS:-60}"

echo "[interop:${SCENARIO}] building magpie-interop image"
docker compose -f "$COMPOSE_FILE" build

echo "[interop:${SCENARIO}] bringing up stack"
docker compose -f "$COMPOSE_FILE" up -d

cleanup() {
  docker compose -f "$COMPOSE_FILE" logs > "/tmp/interop-${SCENARIO}.log" 2>&1 || true
  docker compose -f "$COMPOSE_FILE" down --volumes --remove-orphans || true
}
trap cleanup EXIT

# Give services a few seconds to initialize before running the gate.
sleep 3
echo "[interop:${SCENARIO}] stack up. Dumping initial container logs..."
docker compose -f "$COMPOSE_FILE" logs --no-color 2>&1 | tail -50
echo "[interop:${SCENARIO}] Running per-client SHA-256 gate..."

GATE="$(dirname "$0")/gate_${GATE_NAME}.sh"
if [ -x "$GATE" ]; then
  INTEROP_DEADLINE_SECS="$DEADLINE_SECS" "$GATE" "$COMPOSE_FILE"
  echo "[interop:${SCENARIO}] PASS"
else
  echo "[interop:${SCENARIO}] per-client gate script missing: $GATE" >&2
  exit 78  # EX_CONFIG — scaffolding incomplete
fi
