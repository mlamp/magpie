#![allow(
    missing_docs,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]
//! Criterion benchmarks for metainfo parsing.
//!
//! Three shapes: v1 single file, v1 multi-file, v2 single file. For each,
//! parsing cost is measured against a torrent representative of a typical
//! real-world download (1 MiB of pieces, either one file or 32 files).
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use magpie_bt_metainfo::parse;

#[path = "../tests/common/mod.rs"]
mod common;

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    let v1 = common::synth_v1_single("big.bin", 1 << 20, 1 << 15);
    group.throughput(Throughput::Bytes(v1.len() as u64));
    group.bench_with_input(BenchmarkId::new("v1_single", v1.len()), &v1, |b, data| {
        b.iter(|| {
            let m = parse(black_box(data)).unwrap();
            black_box(m);
        });
    });

    let owned_paths: Vec<(u64, Vec<&str>)> = (0..32_usize)
        .map(|i| (32_768_u64, vec!["a", "b", "c"][..=(i % 3)].to_vec()))
        .collect();
    let file_slices: Vec<(u64, &[&str])> = owned_paths
        .iter()
        .map(|(l, p)| (*l, p.as_slice()))
        .collect();
    let v1_multi = common::synth_v1_multi("pack", &file_slices, 1 << 15);
    group.throughput(Throughput::Bytes(v1_multi.len() as u64));
    group.bench_with_input(
        BenchmarkId::new("v1_multi", v1_multi.len()),
        &v1_multi,
        |b, data| {
            b.iter(|| {
                let m = parse(black_box(data)).unwrap();
                black_box(m);
            });
        },
    );

    let v2 = common::synth_v2_single("big.v2", 1 << 20, 1 << 14);
    group.throughput(Throughput::Bytes(v2.len() as u64));
    group.bench_with_input(BenchmarkId::new("v2_single", v2.len()), &v2, |b, data| {
        b.iter(|| {
            let m = parse(black_box(data)).unwrap();
            black_box(m);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
