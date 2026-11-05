//! FR-015 benches. Budgets on the TCL G08 (armv7): plain cue change p95
//! <= 4 ms, idle frame = no work.

use criterion::{criterion_group, criterion_main, Criterion};
use pharos_subs::{Config, Renderer, Script, Viewport};
use std::hint::black_box;

const FONT: &[u8] = include_bytes!("../tests/fonts/LiberationSans-Regular.ttf");

/// Two alternating two-line cues, so with a 1-byte event cache every frame
/// is a miss that does layout + raster.
const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nI told you we should have taken the other road,\nbut nobody ever listens to me anymore.\n\n\
2\n00:00:04,000 --> 00:00:06,000\nThe harbour lights were already going out\nwhen the last ferry pulled away.\n";

fn renderer(cfg: Config) -> Renderer {
    let mut r = Renderer::new(cfg);
    if r.add_font(FONT.to_vec()).is_err() {
        panic!("bench font must load");
    }
    r.load(Script::parse(SRT));
    r
}

fn benches(c: &mut Criterion) {
    let vp = Viewport::full(1920, 1080);

    // Event cache disabled, glyph cache warm: the steady-state cost of a
    // new cue (shaping, wrap, glyph blit, backdrop, mask allocation).
    let mut r = renderer(Config {
        event_cache_bytes: 1,
        glyph_cache_bytes: 8 << 20,
    });
    let mut flip = false;
    c.bench_function("plain_cue_change", |b| {
        b.iter(|| {
            flip = !flip;
            let t = if flip { 2000 } else { 5000 };
            black_box(r.frame(black_box(t), &vp))
        });
    });

    // Worst case: glyph cache also cold (every glyph outline rasterised).
    let mut cold = renderer(Config {
        event_cache_bytes: 1,
        glyph_cache_bytes: 1,
    });
    c.bench_function("plain_cue_change_cold_glyphs", |b| {
        b.iter(|| {
            flip = !flip;
            let t = if flip { 2000 } else { 5000 };
            black_box(cold.frame(black_box(t), &vp))
        });
    });

    // Same cue again with default caches: the per-frame cost while a cue shows.
    let mut idle = renderer(Config::default());
    idle.frame(2000, &vp);
    c.bench_function("idle_frame", |b| {
        b.iter(|| black_box(idle.frame(black_box(2000), &vp)));
    });
}

criterion_group!(group, benches);
criterion_main!(group);
