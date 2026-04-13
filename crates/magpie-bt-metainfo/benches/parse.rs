#![allow(missing_docs)]
//! Criterion benchmark skeleton for metainfo parsing.
use criterion::{Criterion, criterion_group, criterion_main};

fn parse_skeleton(c: &mut Criterion) {
    c.bench_function("parse_skeleton", |b| b.iter(|| {}));
}

criterion_group!(benches, parse_skeleton);
criterion_main!(benches);
