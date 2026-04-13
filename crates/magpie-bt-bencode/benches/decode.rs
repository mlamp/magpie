#![allow(missing_docs)]
//! Criterion benchmark skeleton for bencode decode.
//!
//! Real workload lands with the decoder implementation. Scaffold only
//! registers a benchmark group so the bench binary compiles and the baseline
//! slot exists.
use criterion::{Criterion, criterion_group, criterion_main};

fn decode_skeleton(c: &mut Criterion) {
    c.bench_function("decode_skeleton", |b| b.iter(|| {}));
}

criterion_group!(benches, decode_skeleton);
criterion_main!(benches);
