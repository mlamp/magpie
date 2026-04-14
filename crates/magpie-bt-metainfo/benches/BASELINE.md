# magpie-bt-metainfo — bench baseline (phase B)

Baseline captured 2026-04-13 on Darwin 25.3.0 (arm64), rustc 1.94.1, release profile (LTO thin, codegen-units=1).

| bench | input size | time (median) | throughput |
|---|---:|---:|---:|
| parse/v1_single | 716 B | 1.24 µs | 550 MiB/s |
| parse/v1_multi (32 files) | 1.7 KiB | 10.15 µs | 159 MiB/s |
| parse/v2_single | 157 B | 1.28 µs | 116 MiB/s |

Notes:
- `v1_single` and `v1_multi` each embed a 1 MiB synthetic pieces blob; the
  throughput figure is dominated by structural parsing plus SHA-1 of the info
  dict (which is the full 1 MiB for v1). Effective structural throughput is
  much higher than reported.
- `v2_single` is small (no `pieces` blob); ~1.28 µs includes SHA-256 over the
  info dict. Future optimisation: deduplicate hash passes for hybrid torrents.
- `v1_multi` cost is higher because the parser builds an owned `Vec<FileV1>`
  including path-component vectors. Still well under 10 µs for 32 files.

## How to reproduce

```sh
cargo bench -p magpie-bt-metainfo --bench parse -- \
  --warm-up-time 1 --measurement-time 2 --sample-size 10 \
  --save-baseline phase-b
```

## Compare

```sh
cargo bench -p magpie-bt-metainfo --bench parse -- --baseline phase-b
```
