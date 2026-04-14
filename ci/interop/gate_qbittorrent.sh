#!/usr/bin/env bash
# Per-client gate for the qBittorrent scenario: add
# fixture.torrent.with-announce via the webUI API, poll for completion,
# SHA-256 the downloaded file against fixture.bin.
#
# Called by run.sh after `docker compose up -d`. All HTTP requests target
# the leech container's mapped webUI port. Credentials default to
# admin/adminadmin (the linuxserver/qbittorrent default on first boot).
#
# Exit: 0 on SHA-256 match within deadline, non-zero otherwise.
set -euo pipefail

COMPOSE_FILE="$1"
DEADLINE_SECS="${INTEROP_DEADLINE_SECS:-120}"
WEB_HOST="${QBT_WEB_HOST:-http://127.0.0.1:8080}"
USER="${QBT_USER:-admin}"
PASS="${QBT_PASS:-adminadmin}"

need() { command -v "$1" >/dev/null || { echo "$1 required" >&2; exit 2; }; }
need curl
need jq
need sha256sum

# Wait for webUI to accept logins — image takes ~20s on cold boot.
# qBittorrent 4.6+ requires the Referer header to match the webUI host
# as an anti-CSRF guard; omit it and the login returns 403 Forbidden.
login() {
  curl -sS --fail -c /tmp/qbt.cookies \
    -H "Referer: $WEB_HOST" \
    --data-urlencode "username=$USER" --data-urlencode "password=$PASS" \
    "$WEB_HOST/api/v2/auth/login"
}
for _ in $(seq 1 60); do
  if login 2>/dev/null | grep -q Ok; then break; fi
  sleep 1
done
login | grep -q Ok || { echo "qBittorrent webUI login failed" >&2; exit 1; }

# Copy the torrent out of the shared volume (leech mounts it read-only
# at /shared/fixture.torrent.with-announce) into /tmp for upload.
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.torrent.with-announce /tmp/upload.torrent
docker compose -f "$COMPOSE_FILE" cp leech:/shared/fixture.bin /tmp/expected.bin

# Upload torrent.
curl -sS --fail -b /tmp/qbt.cookies \
  -H "Referer: $WEB_HOST" \
  -F "torrents=@/tmp/upload.torrent" \
  -F "savepath=/downloads" \
  "$WEB_HOST/api/v2/torrents/add"

# Poll until state == "completed" or deadline.
deadline=$(( $(date +%s) + DEADLINE_SECS ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  state=$(curl -sS -b /tmp/qbt.cookies -H "Referer: $WEB_HOST" "$WEB_HOST/api/v2/torrents/info" \
          | jq -r '.[0].state // "unknown"')
  progress=$(curl -sS -b /tmp/qbt.cookies -H "Referer: $WEB_HOST" "$WEB_HOST/api/v2/torrents/info" \
          | jq -r '.[0].progress // 0')
  echo "[qbt] state=$state progress=$progress"
  case "$state" in
    uploading|stalledUP|pausedUP|checkingUP|completed) break ;;
    error|missingFiles) echo "fatal state: $state" >&2; exit 1 ;;
  esac
  sleep 2
done
[ "$(date +%s)" -lt "$deadline" ] || { echo "deadline exceeded" >&2; exit 1; }

# Pull the downloaded file out of the leech container and SHA-256 it.
docker compose -f "$COMPOSE_FILE" cp leech:/downloads/interop.bin /tmp/got.bin
expected_sha=$(sha256sum /tmp/expected.bin | awk '{print $1}')
got_sha=$(sha256sum /tmp/got.bin | awk '{print $1}')
echo "[qbt] expected_sha=$expected_sha got_sha=$got_sha"
[ "$expected_sha" = "$got_sha" ] || {
  echo "SHA-256 mismatch" >&2
  exit 1
}
echo "[qbt] PASS"
