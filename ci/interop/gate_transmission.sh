#!/usr/bin/env bash
# Per-client gate for the Transmission scenario: add
# fixture.torrent.with-announce via the RPC API, poll for completion,
# SHA-256 the downloaded file against fixture.bin.
#
# Transmission's RPC requires a session-id header (anti-CSRF). Fetch one
# from a 409 response, then include on subsequent calls. If a call gets
# a fresh 409, refresh the session ID and retry.
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

# Fetch session ID from a 409 response header.
refresh_session_id() {
  local hdr
  hdr=$(mktemp)
  # Transmission returns 409 with the session ID header on any request
  # without a valid session ID. curl exits 0 (no --fail).
  curl -sS -D "$hdr" -o /dev/null $AUTH "$RPC_HOST" 2>/dev/null || true
  sid=$(grep -i "X-Transmission-Session-Id" "$hdr" | awk '{print $2}' | tr -d '\r\n' || true)
  rm -f "$hdr"
}

# RPC call with auto session-ID refresh on 409.
tm_rpc() {
  local body="$1"
  local attempt
  for attempt in 1 2 3; do
    local http_code
    # -o sends body to file; -w writes ONLY the status code to stdout.
    http_code=$(curl -sS \
      -o /tmp/tm_rpc_out.json \
      -w '%{http_code}' \
      $AUTH \
      -H "X-Transmission-Session-Id: $sid" \
      -H "Content-Type: application/json" \
      -d "$body" \
      "$RPC_HOST" 2>/dev/null) || true
    echo "[tm] RPC attempt $attempt: HTTP $http_code" >&2
    if [ "$http_code" = "409" ]; then
      echo "[tm] refreshing session ID" >&2
      refresh_session_id
      echo "[tm] new session ID: ${sid:0:8}..." >&2
      continue
    fi
    if [ "$http_code" = "200" ]; then
      cat /tmp/tm_rpc_out.json
      return 0
    fi
    echo "[tm] unexpected HTTP $http_code" >&2
    sleep 1
  done
  echo "[tm] RPC failed after 3 attempts" >&2
  return 1
}

# Wait for Transmission RPC — cold boot takes 30-90s on CI runners.
sid=""
for _ in $(seq 1 90); do
  refresh_session_id
  [ -n "$sid" ] && break
  sleep 1
done
[ -n "$sid" ] || { echo "Transmission RPC not reachable after 90s" >&2; exit 1; }
echo "[tm] got session ID: ${sid:0:8}..."

# Copy fixtures out of the shared volume.
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.torrent.with-announce /tmp/upload.torrent
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.bin /tmp/expected.bin

# Upload torrent as base64.
b64=$(base64 -i /tmp/upload.torrent | tr -d '\n')
body=$(jq -n --arg m "$b64" '{method:"torrent-add", arguments:{metainfo:$m, "download-dir":"/downloads"}}')
tm_rpc "$body" >/dev/null
echo "[tm] torrent added"

# Poll torrent-get until percentDone == 1.0 or deadline.
deadline=$(( $(date +%s) + DEADLINE_SECS ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  q=$(jq -n '{method:"torrent-get", arguments:{fields:["percentDone","status"]}}')
  resp=$(tm_rpc "$q")
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
