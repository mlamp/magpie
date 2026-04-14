# 0008 — Vectorised file I/O for `FileStorage`

- **Status**: proposed (placeholder)
- **Date**: 2026-04-13

## Context

M0 phase C shipped `FileStorage` using `std::os::unix::fs::FileExt::read_at` / `write_at` (and the Windows equivalents) — one positional syscall per block. These are safe stdlib wrappers over `pread` / `pwrite`.

Piece verification and peer-driven writes frequently hit multiple non-contiguous offsets in quick succession. At gigabit-plus throughput with 16 KiB blocks, the scalar path translates a single piece write into ~64 `pwrite` syscalls. The equivalent vectorised call (`pwritev`) would do it in one syscall, saving ~10–30× in syscall overhead alone.

## Decision (to be made)

Introduce a vectorised `writev` / `readv` implementation in `magpie-bt-core::storage::FileStorage` using `libc::pwritev` / `libc::preadv` on Unix, placed behind the `magpie-bt-core` `unsafe` allowlist per DISCIPLINES.md. The call site will:

- validate `IoVec` / `IoVecMut` lengths and offsets before the `unsafe` block;
- construct a `libc::iovec` array pinned by a local `[iovec; N]` buffer with `SAFETY:` covering (a) pointer validity, (b) alignment, (c) the kernel's `iovcnt` limit (`IOV_MAX`);
- fall back to a sequential loop when `iovcnt > IOV_MAX` or when any single segment exceeds `isize::MAX`.

## Research (to do)

- Measure actual syscall cost on representative workloads (8–64 vectors per call) against the scalar baseline in `benches/BASELINE.md`.
- Compare with `tokio-uring` / `io_uring` approach; decide whether a blocking vectored call via a worker pool is sufficient for M1, or whether `io_uring` integration should land at the same time.
- Confirm Windows fallback: no direct `pwritev` equivalent; use scheduled scalar calls (current behaviour) or overlapped I/O with a shared completion port.

## Consequences (if accepted)

Positive:

- Reduced syscall count on piece writes at high throughput.
- Larger contiguous kernel → disk transfer hints to the I/O scheduler.

Negative:

- First `unsafe` block in the crate; expands fuzz surface and review burden.
- Per-call iovec array on the stack adds a small alignment constraint.

## Open questions

- Keep Unix-only and document Windows as scalar-only, or add a `tokio-uring` branch?
- Does our block size (default 16 KiB) actually benefit from vectoring, or is the benefit swamped by filesystem prefetch? Needs measurement.

This ADR is a placeholder until measurement decides the outcome. `magpie-bt-core` is `unsafe`-free as of M0 phase C.
