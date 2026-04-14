#![allow(missing_docs, clippy::cast_possible_truncation, clippy::cast_lossless)]
//! Criterion benchmarks for the alert ring (ADR-0002).
//!
//! Measures producer push throughput and drain throughput at a realistic
//! capacity (4 KiB alerts per queue) and under overflow conditions.
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};

fn bench_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("alert_ring/push");
    for cap in [64_usize, 4096] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("cap", cap), &cap, |b, &cap| {
            let q = AlertQueue::new(cap);
            let mut piece = 0_u32;
            b.iter(|| {
                q.push(black_box(Alert::PieceCompleted { piece }));
                piece = piece.wrapping_add(1);
            });
        });
    }
    group.finish();
}

fn bench_push_overflow(c: &mut Criterion) {
    // Queue capacity 4; push 1024 alerts (overflow), then measure cost per push
    // under permanent overflow (exercises the drop-oldest path).
    let mut group = c.benchmark_group("alert_ring/overflow_push");
    group.throughput(Throughput::Elements(1));
    group.bench_function("cap_4", |b| {
        let q = AlertQueue::new(4);
        for i in 0..4 {
            q.push(Alert::PieceCompleted { piece: i });
        }
        let mut piece = 100_u32;
        b.iter(|| {
            q.push(black_box(Alert::PieceCompleted { piece }));
            piece = piece.wrapping_add(1);
        });
    });
    group.finish();
}

fn bench_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group("alert_ring/drain");
    for n in [16_usize, 256, 4096] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("n", n), &n, |b, &n| {
            let q = AlertQueue::new(n);
            for i in 0..n as u32 {
                q.push(Alert::PieceCompleted { piece: i });
            }
            b.iter_batched(
                || {
                    for i in 0..n as u32 {
                        q.push(Alert::PieceCompleted { piece: i });
                    }
                },
                |()| {
                    let batch = q.drain();
                    black_box(batch);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_masked_push(c: &mut Criterion) {
    // With PIECE mask only, PEER alerts are rejected at push — pays the mask
    // check but no buffer write.
    let mut group = c.benchmark_group("alert_ring/masked_push");
    group.throughput(Throughput::Elements(1));
    group.bench_function("rejected", |b| {
        let q = AlertQueue::with_mask(64, AlertCategory::PIECE);
        b.iter(|| {
            q.push(black_box(Alert::PeerConnected { peer: 1 }));
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_push,
    bench_push_overflow,
    bench_drain,
    bench_masked_push
);
criterion_main!(benches);
