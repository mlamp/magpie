# magpie-bt-core — bench baseline (phase C)

Baseline captured 2026-04-13 on Darwin 25.3.0 (arm64), rustc 1.94.1.

## Alert ring (`alert_ring`)

| bench | time (median) | throughput |
|---|---:|---:|
| push/cap_64 | ~9.5 ns | 105 Melem/s |
| push/cap_4096 | ~9.5 ns | 105 Melem/s |
| overflow_push/cap_4 | 9.6 ns | 104 Melem/s |
| drain/n_16 | 10.3 ns | 1.55 Gelem/s |
| drain/n_256 | 10.6 ns | 24.0 Gelem/s |
| drain/n_4096 | 30 ns | 133 Gelem/s |
| masked_push/rejected | 8.7 ns | 114 Melem/s |

Per-push cost (~9 ns) is dominated by `std::sync::Mutex` acquire/release; drain
is effectively a `VecDeque::drain` into a Vec plus swap. Future optimisation:
lock-free SPSC ring if profiling shows mutex contention under real engine load.

## How to reproduce

```sh
cargo bench -p magpie-bt-core --bench alert_ring -- \
  --warm-up-time 1 --measurement-time 2 --sample-size 10 \
  --save-baseline phase-c
```

## Picker (`picker`)

Baseline for the piece picker lands with C4.
