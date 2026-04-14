# RSS budget for M2 weekly soak

**Status: methodology + harness complete — empirical budget populates on first CI run.** The dhat-instrumented soak binary (`crates/magpie-bt-core/examples/dhat_soak.rs`) and CI harness (`ci/soak/dhat.sh`) are wired. The budget table below will be filled after the first successful weekly-soak dhat job.

## What this document is

The number a `weekly-soak` run must come in under for the multi-torrent
endurance gate (M2 `002-seeder-multi-torrent.md` §gate criterion 3,
"24 h multi-torrent soak ... within documented RSS budget").

## Methodology

When the dhat soak binary lands:

1. Run `SOAK_DURATION_SECS=86400 SOAK_PAIRS=8 SOAK_LARGE_PIECE_COUNT=100000 ci/soak/dhat.sh`
   on the same Ubuntu CI runner the weekly job uses (`ubuntu-latest`).
2. Sample peak RSS via `getrusage(RUSAGE_SELF, ru_maxrss)` snapshots
   captured every 60 s by the soak binary itself; persist as a JSON
   sidecar (`peak-rss.json`).
3. Take the maximum across the run as the **observed peak**.
4. Set the budget at `1.25 × observed_peak`, rounded up to the next
   100 MiB. The 25% headroom absorbs Linux page-cache pressure and
   tokio's per-task scratch growth without making the gate flap.
5. Land the number in this document with the date, runner spec, and
   git rev of the run.

## Budget table

| Date | Git rev | Runner | Observed peak | Budget |
|------|---------|--------|--------------:|-------:|
| TBD  | TBD     | ubuntu-latest | TBD     | TBD    |

## What blows the budget

Likely culprits if a future run regresses past the documented number:

- **Read-cache growth**: `ReadCache` is bounded (ADR-0018 default 64 MiB)
  but if the bound is mis-applied or per-torrent caches accumulate,
  expect ~64 MiB × torrents extra.
- **Disk-writer queue depth**: ADR-0007 caps at 64 MiB session-wide.
  Mis-configured per-torrent buffers would multiply.
- **Per-peer `PeerUploadQueue`**: 4 MiB watermark × peers (ADR-0017
  upper bound). A misconfigured cap is a real exposure.
- **Pending alerts**: ring is bounded by `AlertQueue::new(cap)` at
  construction; runaway producers should drop, not grow.
- **Synthetic content fixtures in tests**: 1 MiB × 8 pairs = 8 MiB
  baseline; the optional 100k-piece pair adds 800 MiB at 8 KiB pieces.
  Tracked separately from the engine RSS.

## Why a budget at all

A weekly soak that simply "ran for 24h" without a quantitative ceiling
is worth less than its runtime cost — `feedback_plan_red_team`
silent-failure trap. The budget makes regressions visible the moment
they cross the line, not when a user notices on production.
