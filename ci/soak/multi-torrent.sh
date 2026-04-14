#!/usr/bin/env bash
# M2 weekly soak harness — multi-torrent endurance run.
#
# Spins up N concurrent magpie engine pairs (seed↔leech over loopback)
# using synthetic torrents from `magpie-bt-metainfo`'s `test-support`
# generator. Drives them for $SOAK_DURATION_SECS, asserting all pairs
# complete every cycle and no piece-verify failures are observed.
#
# Invoked by `.github/workflows/weekly-soak.yml`. Defaults are tuned for
# a ~24h run on a standard CI runner; can be shortened for local
# verification with SOAK_DURATION_SECS=60.
#
# Requires: a Unix runner with /tmp on tmpfs (or equivalent fast scratch),
# rustc nightly+stable both available. The test itself runs under stable.
#
# Exit status: 0 on clean completion, non-zero on any failed cycle or
# magpie panic. Captured stderr is the failure-investigation surface.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# Tuneables. Keep defaults conservative; weekly cron overrides via env.
: "${SOAK_DURATION_SECS:=86400}"   # 24 h
: "${SOAK_PAIRS:=8}"                # ≥8 per gate criterion 3
: "${SOAK_LARGE_PIECE_COUNT:=100000}" # ADR-0005 linear-picker exercise

export SOAK_DURATION_SECS SOAK_PAIRS SOAK_LARGE_PIECE_COUNT

echo "[soak] duration=${SOAK_DURATION_SECS}s pairs=${SOAK_PAIRS} large_pieces=${SOAK_LARGE_PIECE_COUNT}"

# Run the ignored integration test in release mode under a generous wall
# timeout so a hung run aborts cleanly within CI's job budget.
WALL_TIMEOUT_SECS=$(( SOAK_DURATION_SECS + 600 ))

timeout --signal=KILL "${WALL_TIMEOUT_SECS}" \
    cargo test --release \
        -p magpie-bt-core \
        --features "" \
        --test soak_multi_torrent \
        -- \
        --ignored \
        --nocapture \
        --test-threads=1
