#!/usr/bin/env bash
# Full 24h M2 soak run — designed to run in tmux on a server.
#
# Runs both the multi-torrent endurance test and the dhat heap profiler
# in sequence (not parallel — they compete for CPU). Captures:
#
#   results/multi-torrent.log   — full stderr from the endurance run
#   results/dhat-heap.json      — dhat allocation profile
#   results/peak-rss.json       — peak RSS + cycle count
#   results/rss-timeline.csv    — RSS sampled every 60s (for graphing)
#   results/summary.txt         — one-page summary for docs/RSS-budget.md
#
# Usage:
#   git clone https://github.com/mlamp/magpie && cd magpie
#   tmux new -s soak './ci/soak/run-24h.sh'
#
# Override defaults via env:
#   SOAK_DURATION_SECS=3600 SOAK_PAIRS=4 ./ci/soak/run-24h.sh
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

: "${SOAK_DURATION_SECS:=86400}"
: "${SOAK_PAIRS:=8}"
: "${SOAK_LARGE_PIECE_COUNT:=100000}"

RESULTS="$ROOT/results-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RESULTS"

GIT_REV=$(git rev-parse --short HEAD)
HOSTNAME=$(hostname)
START=$(date -u +%Y-%m-%dT%H:%M:%SZ)

cat <<EOF | tee "$RESULTS/summary.txt"
=== magpie 24h soak run ===
  started:    $START
  git rev:    $GIT_REV
  host:       $HOSTNAME
  duration:   ${SOAK_DURATION_SECS}s ($(( SOAK_DURATION_SECS / 3600 ))h)
  pairs:      $SOAK_PAIRS
  large pcs:  $SOAK_LARGE_PIECE_COUNT
  results:    $RESULTS
EOF

echo ""
echo "[soak] building release binaries..."
cargo build --release -p magpie-bt-core --features dhat-heap --example dhat_soak 2>&1 | tail -3
cargo test --release -p magpie-bt-core --features "" --test soak_multi_torrent --no-run 2>&1 | tail -3
echo "[soak] build complete."
echo ""

# ── Phase 1: Multi-torrent endurance ──────────────────────────────────
echo "[soak] === Phase 1: multi-torrent endurance ($SOAK_PAIRS pairs, ${SOAK_DURATION_SECS}s) ==="

# Background RSS sampler — polls /proc/self or getrusage every 60s
(
  echo "timestamp,elapsed_s,rss_kib" > "$RESULTS/rss-timeline.csv"
  START_EPOCH=$(date +%s)
  while true; do
    NOW=$(date +%s)
    ELAPSED=$(( NOW - START_EPOCH ))
    if [ -f /proc/self/status ]; then
      # Linux: VmRSS from /proc
      RSS=$(awk '/^VmRSS:/ {print $2}' /proc/self/status 2>/dev/null || echo 0)
    else
      # macOS fallback: use ps
      RSS=$(ps -o rss= -p $$ 2>/dev/null | tr -d ' ' || echo 0)
    fi
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),$ELAPSED,$RSS" >> "$RESULTS/rss-timeline.csv"
    sleep 60
  done
) &
RSS_PID=$!
trap 'kill $RSS_PID 2>/dev/null || true' EXIT

WALL_TIMEOUT=$(( SOAK_DURATION_SECS + 600 ))

SOAK_DURATION_SECS=$SOAK_DURATION_SECS \
SOAK_PAIRS=$SOAK_PAIRS \
SOAK_LARGE_PIECE_COUNT=$SOAK_LARGE_PIECE_COUNT \
timeout --signal=KILL "$WALL_TIMEOUT" \
    cargo test --release \
        -p magpie-bt-core \
        --features "" \
        --test soak_multi_torrent \
        -- \
        --ignored \
        --nocapture \
        --test-threads=1 \
    2>&1 | tee "$RESULTS/multi-torrent.log"

MT_EXIT=${PIPESTATUS[0]}
echo ""
echo "[soak] multi-torrent exit code: $MT_EXIT"

# ── Phase 2: dhat heap profiler ───────────────────────────────────────
echo ""
echo "[soak] === Phase 2: dhat heap profile ($SOAK_PAIRS pairs, ${SOAK_DURATION_SECS}s) ==="

cd "$RESULTS"
SOAK_DURATION_SECS=$SOAK_DURATION_SECS \
SOAK_PAIRS=$SOAK_PAIRS \
    "$ROOT/target/release/examples/dhat_soak" 2>&1 | tee dhat.log

DHAT_EXIT=${PIPESTATUS[0]}
echo ""
echo "[soak] dhat exit code: $DHAT_EXIT"

# Move dhat outputs to results dir (dhat_soak writes to cwd)
# They're already there since we cd'd to $RESULTS

# ── Summary ───────────────────────────────────────────────────────────
kill $RSS_PID 2>/dev/null || true

END=$(date -u +%Y-%m-%dT%H:%M:%SZ)
PEAK_RSS="unknown"
CYCLES="unknown"
if [ -f "$RESULTS/peak-rss.json" ]; then
  PEAK_RSS=$(grep peak_rss_kib "$RESULTS/peak-rss.json" | tr -dc '0-9')
  CYCLES=$(grep cycles "$RESULTS/peak-rss.json" | tr -dc '0-9')
fi

# Compute budget: 1.25 × peak, rounded up to next 100 MiB
if [ "$PEAK_RSS" != "unknown" ] && [ -n "$PEAK_RSS" ]; then
  BUDGET_KIB=$(( (PEAK_RSS * 125 / 100 + 102399) / 102400 * 102400 ))
  BUDGET_MIB=$(( BUDGET_KIB / 1024 ))
  PEAK_MIB=$(( PEAK_RSS / 1024 ))
else
  BUDGET_KIB="?"
  BUDGET_MIB="?"
  PEAK_MIB="?"
fi

cat <<EOF | tee -a "$RESULTS/summary.txt"

=== results ===
  finished:       $END
  multi-torrent:  $([ "$MT_EXIT" -eq 0 ] && echo "PASS" || echo "FAIL (exit $MT_EXIT)")
  dhat profile:   $([ "$DHAT_EXIT" -eq 0 ] && echo "PASS" || echo "FAIL (exit $DHAT_EXIT)")
  dhat cycles:    $CYCLES
  peak RSS:       $PEAK_RSS KiB ($PEAK_MIB MiB)
  budget (1.25×): $BUDGET_KIB KiB ($BUDGET_MIB MiB)

=== for docs/RSS-budget.md ===
| $END | $GIT_REV | $HOSTNAME | $PEAK_MIB MiB | $BUDGET_MIB MiB |

=== files ===
  $RESULTS/multi-torrent.log
  $RESULTS/dhat.log
  $RESULTS/dhat-heap.json       (view at https://nnethercote.github.io/dh_view/dh_view.html)
  $RESULTS/peak-rss.json
  $RESULTS/rss-timeline.csv
  $RESULTS/summary.txt
EOF

echo ""
if [ "$MT_EXIT" -eq 0 ] && [ "$DHAT_EXIT" -eq 0 ]; then
  echo "[soak] ALL PASS. Copy the RSS-budget.md row above into docs/RSS-budget.md."
  exit 0
else
  echo "[soak] FAILURE — check logs above."
  exit 1
fi
