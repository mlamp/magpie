#![allow(missing_docs)]
//! Criterion benchmark skeleton for the piece picker.
use criterion::{Criterion, criterion_group, criterion_main};

fn picker_skeleton(c: &mut Criterion) {
    c.bench_function("picker_skeleton", |b| b.iter(|| {}));
}

criterion_group!(benches, picker_skeleton);
criterion_main!(benches);
