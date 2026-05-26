#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Bench for stable path hash — guards SIMD xxh3 regressions.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pharos_scanner::fs::stable_id;
use std::path::Path;

fn bench_hash(c: &mut Criterion) {
    let short = Path::new("/srv/media/movie.mkv");
    let long = Path::new(
        "/srv/media/shows/Some Long Show Name (2024)/Season 03/\
         S03E07 - Episode With A Very Long Title Goes Here.mkv",
    );
    let mut g = c.benchmark_group("stable_id");
    g.bench_function("short_path", |b| b.iter(|| stable_id(black_box(short))));
    g.bench_function("long_path", |b| b.iter(|| stable_id(black_box(long))));
    g.finish();
}

criterion_group!(benches, bench_hash);
criterion_main!(benches);
