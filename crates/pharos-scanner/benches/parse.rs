#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Criterion bench for `parse_ffprobe_output` — guards SIMD JSON regressions.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pharos_scanner::parse_ffprobe_output;

const SMALL: &[u8] = br#"{
    "streams": [{"codec_type":"audio"}],
    "format": {"format_name":"flac","duration":"245.123"}
}"#;

const LARGE: &[u8] = br#"{
    "streams": [
        {"codec_type":"video"},
        {"codec_type":"audio"},
        {"codec_type":"audio"},
        {"codec_type":"subtitle"},
        {"codec_type":"subtitle"},
        {"codec_type":"subtitle"},
        {"codec_type":"subtitle"}
    ],
    "format": {
        "format_name":"matroska,webm",
        "duration":"7384.245",
        "size":"4823145000",
        "bit_rate":"5219800",
        "nb_streams":"7",
        "nb_programs":"0",
        "tags":{"title":"Movie","encoder":"libebml v1.4.2 + libmatroska v1.6.4"}
    }
}"#;

fn bench_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("ffprobe_parse");
    g.bench_function("small_audio", |b| {
        b.iter(|| parse_ffprobe_output(black_box(SMALL)).unwrap())
    });
    g.bench_function("large_video", |b| {
        b.iter(|| parse_ffprobe_output(black_box(LARGE)).unwrap())
    });
    g.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
