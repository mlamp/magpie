#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
//! Criterion benchmarks for the piece picker.
//!
//! Measures [`Picker::pick`], [`Picker::pick_n`], and a progress loop
//! (`pick` + `mark_have` until complete) at realistic piece counts (256, 4096).
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use magpie_bt_core::picker::Picker;

fn seeded(piece_count: u32, peer_count: u32) -> Picker {
    let mut p = Picker::new(piece_count);
    // Each peer advertises a contiguous slice of 1/4 of the torrent, offset by
    // (i * piece_count / peer_count). Gives a moderately skewed distribution.
    for i in 0..peer_count {
        let start = (u64::from(i) * u64::from(piece_count) / u64::from(peer_count)) as u32;
        let span = piece_count / 4;
        let bits: Vec<bool> = (0..piece_count)
            .map(|idx| {
                let rel = idx.wrapping_sub(start);
                rel < span
            })
            .collect();
        p.observe_peer_bitfield(&bits);
    }
    p
}

fn bench_pick(c: &mut Criterion) {
    let mut group = c.benchmark_group("picker/pick");
    for n in [256_u32, 4096] {
        let p = seeded(n, 32);
        group.bench_with_input(BenchmarkId::new("pieces", n), &p, |b, p| {
            b.iter(|| {
                let v = p.pick();
                black_box(v);
            });
        });
    }
    group.finish();
}

fn bench_pick_n(c: &mut Criterion) {
    let mut group = c.benchmark_group("picker/pick_n");
    for n in [256_u32, 4096] {
        let p = seeded(n, 32);
        group.bench_with_input(BenchmarkId::new("pieces", n), &p, |b, p| {
            b.iter(|| {
                let v = p.pick_n(32);
                black_box(v);
            });
        });
    }
    group.finish();
}

fn bench_progress(c: &mut Criterion) {
    let mut group = c.benchmark_group("picker/progress");
    for n in [256_u32, 4096] {
        group.bench_with_input(BenchmarkId::new("pieces", n), &n, |b, &n| {
            b.iter_batched(
                || seeded(n, 32),
                |mut p| {
                    // Pretend every piece is available from at least one peer.
                    for i in 0..n {
                        p.mark_have(i);
                    }
                    black_box(p.missing_count());
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_pick, bench_pick_n, bench_progress);
criterion_main!(benches);
