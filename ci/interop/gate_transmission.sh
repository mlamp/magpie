#!/usr/bin/env bash
# Per-client gate for the Transmission scenario: add
# fixture.torrent.with-announce via the RPC API, poll for completion,
# SHA-256 the downloaded file against fixture.bin.
#
# Transmission's RPC requires a session-id header (anti-CSRF). Fetch one
# from a 409 response, then include on subsequent calls.
#
# Exit: 0 on SHA-256 match within deadline, non-zero otherwise.
set -euo pipefail

COMPOSE_FILE="$1"
DEADLINE_SECS="${INTEROP_DEADLINE_SECS:-120}"
RPC_HOST="${TRANSMISSION_RPC_HOST:-http://127.0.0.1:9091/transmission/rpc}"
USER="${TRANSMISSION_USER:-admin}"
PASS="${TRANSMISSION_PASS:-admin}"

need() { command -v "$1" >/dev/null || { echo "$1 required" >&2; exit 2; }; }
need curl
need jq
need sha256sum
need base64

AUTH="-u $USER:$PASS"

session_id() {
  # 409 Conflict response carries X-Transmission-Session-Id.
  curl -sS -D - $AUTH -o /dev/null "$RPC_HOST" \
    | grep -i "X-Transmission-Session-Id" \
    | awk '{print $2}' | tr -d '\r\n'
}

# Wait for Transmission RPC.
for _ in $(seq 1 60); do
  sid=$(session_id || true)
  [ -n "$sid" ] && break
  sleep 1
done
[ -n "$sid" ] || { echo "Transmission RPC not reachable" >&2; exit 1; }

# Upload torrent as base64.
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.torrent.with-announce /tmp/upload.torrent
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.bin /tmp/expected.bin
b64=$(base64 -i /tmp/upload.torrent | tr -d '\n')
body=$(jq -n --arg m "$b64" '{method:"torrent-add", arguments:{metainfo:$m, "download-dir":"/downloads"}}')
curl -sS --fail $AUTH -H "X-Transmission-Session-Id: $sid" \
  -H "Content-Type: application/json" -d "$body" "$RPC_HOST" >/dev/null

# Poll torrent-get until percentDone == 1.0 or deadline.
deadline=$(( $(date +%s) + DEADLINE_SECS ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  q=$(jq -n '{method:"torrent-get", arguments:{fields:["percentDone","status"]}}')
  resp=$(curl -sS $AUTH -H "X-Transmission-Session-Id: $sid" \
    -H "Content-Type: application/json" -d "$q" "$RPC_HOST")
  done_frac=$(echo "$resp" | jq -r '.arguments.torrents[0].percentDone // 0')
  status=$(echo "$resp" | jq -r '.arguments.torrents[0].status // -1')
  echo "[tm] percentDone=$done_frac status=$status"
  # Transmission status 6 = seeding; percentDone should be 1.0 by then.
  if [ "$status" = "6" ]; then break; fi
  sleep 2
done
[ "$(date +%s)" -lt "$deadline" ] || { echo "deadline exceeded" >&2; exit 1; }

docker compose -f "$COMPOSE_FILE" cp leech:/downloads/interop.bin /tmp/got.bin
expected_sha=$(sha256sum /tmp/expected.bin | awk '{print $1}')
got_sha=$(sha256sum /tmp/got.bin | awk '{print $1}')
echo "[tm] expected_sha=$expected_sha got_sha=$got_sha"
[ "$expected_sha" = "$got_sha" ] || {
  echo "SHA-256 mismatch" >&2
  exit 1
}
echo "[tm] PASS"
