#!/usr/bin/env bash
# Per-client gate for the Transmission magnet scenario: torrent-add via
# magnet URI rather than .torrent metainfo. Drives BEP 9 ut_metadata
# end-to-end against magpie's metadata-server path.
#
# Exit: 0 on SHA-256 match within deadline, non-zero otherwise.
set -euo pipefail

COMPOSE_FILE="$1"
DEADLINE_SECS="${INTEROP_DEADLINE_SECS:-180}"
RPC_HOST="${TRANSMISSION_RPC_HOST:-http://127.0.0.1:9091/transmission/rpc}"
USER="${TRANSMISSION_USER:-admin}"
PASS="${TRANSMISSION_PASS:-admin}"

need() { command -v "$1" >/dev/null || { echo "$1 required" >&2; exit 2; }; }
need curl
need jq
need sha256sum

AUTH="-u $USER:$PASS"

refresh_session_id() {
  local resp
  resp=$(curl -sS -i $AUTH "$RPC_HOST" 2>/dev/null || true)
  sid=$(echo "$resp" | sed -n 's/^[Xx]-[Tt]ransmission-[Ss]ession-[Ii]d: *\([^ ]*\).*/\1/p' | tr -d '\r\n')
}

tm_rpc() {
  local body="$1"
  local attempt
  for attempt in 1 2 3; do
    local http_code
    http_code=$(curl -sS \
      -o /tmp/tm_rpc_out.json \
      -w '%{http_code}' \
      $AUTH \
      -H "X-Transmission-Session-Id: $sid" \
      -H "Content-Type: application/json" \
      -d "$body" \
      "$RPC_HOST" 2>/dev/null) || true
    echo "[tm-magnet] RPC attempt $attempt: HTTP $http_code (sid=${sid:0:8})" >&2
    if [ "$http_code" = "409" ]; then
      refresh_session_id
      continue
    fi
    if [ "$http_code" = "200" ]; then
      cat /tmp/tm_rpc_out.json
      return 0
    fi
    echo "[tm-magnet] unexpected HTTP $http_code" >&2
    sleep 1
  done
  echo "[tm-magnet] RPC failed after 3 attempts" >&2
  return 1
}

sid=""
for _ in $(seq 1 90); do
  refresh_session_id
  [ -n "$sid" ] && break
  sleep 1
done
[ -n "$sid" ] || { echo "Transmission RPC not reachable after 90s" >&2; exit 1; }
echo "[tm-magnet] got session ID: '${sid}' (len=${#sid})"

# Read the magnet URI + expected data out of the shared volume.
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.magnet /tmp/fixture.magnet
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.bin /tmp/expected.bin
MAGNET_URI=$(cat /tmp/fixture.magnet)
echo "[tm-magnet] magnet=${MAGNET_URI}"

# torrent-add via magnet URI uses the `filename` argument (not metainfo).
body=$(jq -n --arg fn "$MAGNET_URI" '{method:"torrent-add", arguments:{filename:$fn, "download-dir":"/downloads"}}')
tm_rpc "$body" >/dev/null
echo "[tm-magnet] torrent added (magnet)"

# Poll torrent-get until percentDone == 1.0 or deadline. Transmission
# status 6 = seeding; we accept that as completion. Magnet flow first
# fetches metadata (status 2 = metadata download, then 4 = downloading).
deadline=$(( $(date +%s) + DEADLINE_SECS ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  q=$(jq -n '{method:"torrent-get", arguments:{fields:["percentDone","status","metadataPercentComplete"]}}')
  resp=$(tm_rpc "$q")
  done_frac=$(echo "$resp" | jq -r '.arguments.torrents[0].percentDone // 0')
  status=$(echo "$resp" | jq -r '.arguments.torrents[0].status // -1')
  meta_pct=$(echo "$resp" | jq -r '.arguments.torrents[0].metadataPercentComplete // 0')
  echo "[tm-magnet] meta=$meta_pct percentDone=$done_frac status=$status"
  if [ "$status" = "6" ]; then break; fi
  sleep 2
done
[ "$(date +%s)" -lt "$deadline" ] || { echo "deadline exceeded" >&2; exit 1; }

docker compose -f "$COMPOSE_FILE" cp leech:/downloads/interop.bin /tmp/got.bin
expected_sha=$(sha256sum /tmp/expected.bin | awk '{print $1}')
got_sha=$(sha256sum /tmp/got.bin | awk '{print $1}')
echo "[tm-magnet] expected_sha=$expected_sha got_sha=$got_sha"
[ "$expected_sha" = "$got_sha" ] || {
  echo "SHA-256 mismatch" >&2
  exit 1
}
echo "[tm-magnet] PASS"
