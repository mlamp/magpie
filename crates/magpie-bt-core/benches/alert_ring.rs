#![allow(missing_docs)]
//! Criterion benchmark skeleton for the custom alert ring (ADR 0002).
use criterion::{Criterion, criterion_group, criterion_main};

fn alert_ring_skeleton(c: &mut Criterion) {
    c.bench_function("alert_ring_skeleton", |b| b.iter(|| {}));
}

criterion_group!(benches, alert_ring_skeleton);
criterion_main!(benches);
