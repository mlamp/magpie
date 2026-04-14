#!/usr/bin/env bash
# M2 weekly soak harness — dhat heap-allocation profiling run.
#
# Builds and runs the dhat-instrumented soak example, then validates that
# dhat-heap.json was produced. The CI workflow uploads it as an artifact.
#
# Env vars (forwarded to the binary):
#   SOAK_DURATION_SECS  — total runtime (default 300)
#   SOAK_PAIRS          — concurrent seed-leech pairs (default 4)
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

: "${SOAK_DURATION_SECS:=300}"
: "${SOAK_PAIRS:=4}"
export SOAK_DURATION_SECS SOAK_PAIRS

echo "[soak/dhat] duration=${SOAK_DURATION_SECS}s pairs=${SOAK_PAIRS}"

# Build in release for realistic allocation patterns.
echo "[soak/dhat] building dhat_soak example..."
cargo build --release -p magpie-bt-core --example dhat_soak --features dhat-heap

# Run the binary. dhat writes dhat-heap.json on drop.
echo "[soak/dhat] running dhat_soak..."
./target/release/examples/dhat_soak

# Validate output.
if [ ! -f dhat-heap.json ]; then
    echo >&2 "[soak/dhat] ERROR: dhat-heap.json was not produced"
    exit 1
fi
echo "[soak/dhat] dhat-heap.json produced ($(wc -c < dhat-heap.json) bytes)"

if [ -f peak-rss.json ]; then
    echo "[soak/dhat] peak-rss.json produced ($(cat peak-rss.json))"
fi

echo "[soak/dhat] done"
