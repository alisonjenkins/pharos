use super::common::*;
use pharos_subs::{Config, MaskKind, Rect, Renderer, Script, SubtitleStyle, Viewport};

fn count(f: &pharos_subs::Frame, k: MaskKind) -> usize {
    f.masks.iter().filter(|m| m.kind == k).count()
}

#[test]
fn plain_two_line_cue_gets_hugging_backdrops_and_fill() {
    let mut r = renderer_with(TWO_LINE_SRT);
    let f = r.frame(2000, &vp());
    assert!(f.changed);
    assert_eq!(f.render_size, (1920, 1080));
    assert_eq!(count(&f, MaskKind::Backdrop), 2);
    assert_eq!(count(&f, MaskKind::Fill), 1);
    assert_eq!(count(&f, MaskKind::Border), 0);
    let b: Vec<_> = f
        .masks
        .iter()
        .filter(|m| m.kind == MaskKind::Backdrop)
        .collect();
    // Text-width, not full-width; second line is longer than the first.
    assert!(b[0].w < 900 && b[1].w < 900, "{} {}", b[0].w, b[1].w);
    assert!(b[0].w < b[1].w);
    // Default backdrop is black at 55%.
    assert_eq!(b[0].tint, [0, 0, 0, 140]);
    // Lines are centred and the box stays above the bottom margin (6%).
    let cx = |m: &pharos_subs::Mask| m.x + m.w as i32 / 2;
    assert!((cx(b[0]) - 960).abs() <= 1 && (cx(b[1]) - 960).abs() <= 1);
    let bottom = b.iter().map(|m| m.y + m.h as i32).max().unwrap();
    assert!(bottom <= 1080 - 64, "bottom {bottom}");
    assert!(bottom >= 1080 - 70, "bottom {bottom}");
    // Backdrop corners are rounded: top-left coverage is zero, centre solid.
    assert_eq!(b[0].coverage[0], 0);
    assert_eq!(
        b[0].coverage[(b[0].h / 2 * b[0].stride + b[0].w / 2) as usize],
        255
    );
    // Fill has ink and uses the text colour.
    let fill = f.masks.iter().find(|m| m.kind == MaskKind::Fill).unwrap();
    assert_eq!(fill.tint, [255, 255, 255, 255]);
    assert!(fill.coverage.iter().filter(|c| **c == 255).count() > 100);
    assert_eq!(fill.stride, fill.w);
}

#[test]
fn idle_frames_do_no_work() {
    let mut r = renderer_with(TWO_LINE_SRT);
    assert!(r.frame(0, &vp()).changed); // nothing active yet: first answer is "empty"
    let f = r.frame(500, &vp());
    assert!(!f.changed && f.masks.is_empty());
    assert!(r.frame(1000, &vp()).changed); // cue starts
    let before = r.stats();
    for t in [1001, 1500, 2999] {
        let f = r.frame(t, &vp());
        assert!(!f.changed && f.masks.is_empty(), "t={t}");
    }
    let after = r.stats();
    assert_eq!(after.events_rastered, before.events_rastered);
    assert_eq!(after.glyphs_rastered, before.glyphs_rastered);
    assert_eq!(after.idle_frames, before.idle_frames + 3);
    assert!(r.frame(3000, &vp()).changed); // cue ends: clear
    assert!(r.frame(3000, &vp()).masks.is_empty());
}

#[test]
fn seeking_back_into_a_seen_cue_hits_the_cache() {
    let mut r = renderer_with(TWO_LINE_SRT);
    let a = r.frame(2000, &vp());
    r.frame(4000, &vp());
    let before = r.stats();
    let b = r.frame(2500, &vp());
    assert!(b.changed);
    assert_eq!(r.stats().events_rastered, before.events_rastered);
    assert_eq!(r.stats().event_cache_hits, before.event_cache_hits + 1);
    assert_eq!(hash_frame(&a), hash_frame(&b));
}

#[test]
fn viewport_or_style_change_redraws() {
    let mut r = renderer_with(TWO_LINE_SRT);
    r.frame(2000, &vp());
    assert!(!r.frame(2000, &vp()).changed);
    assert!(r.frame(2000, &Viewport::full(1280, 720)).changed);
    r.frame(2000, &vp());
    let before = hash_frame(&r.frame(2000, &vp()));
    r.set_style(SubtitleStyle {
        size_scale: 1.5,
        ..SubtitleStyle::default()
    });
    let f = r.frame(2000, &vp());
    assert!(f.changed);
    assert_ne!(hash_frame(&f), before);
}

#[test]
fn masks_are_deterministic_across_renderers() {
    let h = |script: &str| {
        let mut r = renderer_with(script);
        hash_frame(&r.frame(2000, &vp()))
    };
    assert_eq!(h(TWO_LINE_SRT), h(TWO_LINE_SRT));
    assert_eq!(h(STYLED_ASS), h(STYLED_ASS));
    assert_ne!(h(TWO_LINE_SRT), h(STYLED_ASS));
}

const STYLED_ASS: &str = "[Script Info]\nPlayResX: 640\nPlayResY: 360\nWrapStyle: 0\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Liberation Sans,28,&H00FFFFFF,&H000000FF,&H00101010,&H80000000,0,0,0,0,100,100,0,0,1,2,1,8,20,20,15,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Top aligned {\\c&H0000FF&\\u1}red{\\r} text\nDialogue: 1,0:00:01.00,0:00:03.00,Default,,0,0,0,,{\\an2\\fs40\\bord0\\shad0}Layered {\\pos(1,1)\\blur3}bottom\n";

#[test]
fn ass_styles_produce_shadow_border_fill_per_event_in_layer_order() {
    let mut r = renderer_with(STYLED_ASS);
    let f = r.frame(2000, &vp());
    // Event 1: shadow + border + two fills (red run and white run);
    // event 2: fill only (bord/shad 0).
    assert_eq!(count(&f, MaskKind::Shadow), 1);
    assert_eq!(count(&f, MaskKind::Border), 1);
    assert_eq!(count(&f, MaskKind::Fill), 3);
    assert_eq!(
        count(&f, MaskKind::Backdrop),
        0,
        "styled ASS has no pharos backdrop"
    );
    let kinds: Vec<_> = f.masks.iter().map(|m| m.kind).collect();
    assert_eq!(&kinds[..2], &[MaskKind::Shadow, MaskKind::Border]);
    let red = f
        .masks
        .iter()
        .find(|m| m.kind == MaskKind::Fill && m.tint == [255, 0, 0, 255]);
    assert!(red.is_some());
    let border = f.masks.iter().find(|m| m.kind == MaskKind::Border).unwrap();
    assert_eq!(border.tint, [0x10, 0x10, 0x10, 255]);
    let shadow = f.masks.iter().find(|m| m.kind == MaskKind::Shadow).unwrap();
    assert_eq!(shadow.tint[3], 127);
    // Shadow shares the border's bitmap (one texture, two quads).
    assert_eq!(shadow.id, border.id);
    // \an8 puts the first event at the top, \an2 the second at the bottom.
    let top = f.masks.iter().find(|m| m.kind == MaskKind::Border).unwrap();
    assert!(top.y < 200);
    let last = f.masks.last().unwrap();
    assert!(last.y > 700, "{}", last.y);
    // Unknown tags were counted, not drawn.
    assert_eq!(r.stats().unsupported_tags.get("pos"), Some(&1));
    assert_eq!(r.stats().unsupported_tags.get("blur"), Some(&1));
}

#[test]
fn long_text_wraps_to_ninety_percent_width() {
    let long = "word ".repeat(40);
    let srt = format!("1\n00:00:00,000 --> 00:00:09,000\n{long}\n");
    let mut r = renderer_with(&srt);
    let f = r.frame(1000, &vp());
    let boxes: Vec<_> = f
        .masks
        .iter()
        .filter(|m| m.kind == MaskKind::Backdrop)
        .collect();
    assert!(boxes.len() >= 2, "{}", boxes.len());
    assert!(boxes.iter().all(|m| m.w as f32 <= 1920.0 * 0.9 + 1.0));
    // Smart wrap keeps lines similar in width (no stub last line).
    let (min, max) = boxes
        .iter()
        .fold((u32::MAX, 0), |(a, b), m| (a.min(m.w), b.max(m.w)));
    assert!(min * 10 >= max * 6, "{min} vs {max}");
}

#[test]
fn wrap_style_2_never_wraps() {
    let words = "word ".repeat(60);
    let ass = STYLED_ASS
        .replace("WrapStyle: 0", "WrapStyle: 2")
        .replace("Top aligned", &words);
    let mut r = renderer_with(&ass);
    let f = r.frame(2000, &vp());
    let fill = f
        .masks
        .iter()
        .filter(|m| m.kind == MaskKind::Fill)
        .map(|m| m.w)
        .max()
        .unwrap();
    assert!(fill > 1920, "single overflowing line, got {fill}");
}

#[test]
fn size_scale_and_controls_inset_move_the_cue() {
    let mut r = renderer_with(TWO_LINE_SRT);
    let h = |f: &pharos_subs::Frame| {
        let b: Vec<_> = f
            .masks
            .iter()
            .filter(|m| m.kind == MaskKind::Backdrop)
            .collect();
        let top = b.iter().map(|m| m.y).min().unwrap();
        let bot = b.iter().map(|m| m.y + m.h as i32).max().unwrap();
        (top, bot)
    };
    let base = h(&r.frame(2000, &vp()));
    let mut with_controls = vp();
    with_controls.bottom_inset = 150;
    let raised = h(&r.frame(2000, &with_controls));
    assert!(raised.1 <= base.1 - 100, "{raised:?} vs {base:?}");

    r.set_style(SubtitleStyle {
        size_scale: 1.5,
        ..SubtitleStyle::default()
    });
    let big = h(&r.frame(2000, &vp()));
    assert!(big.1 - big.0 > (base.1 - base.0) * 13 / 10);
}

#[test]
fn render_is_capped_at_1080p_and_reports_scale() {
    let mut r = renderer_with(TWO_LINE_SRT);
    let f = r.frame(2000, &Viewport::full(3840, 2160));
    assert_eq!(f.render_size, (1920, 1080));
    assert!((f.scale - 2.0).abs() < 1e-6);
    let small = r.frame(2000, &Viewport::full(1280, 720));
    assert_eq!(small.render_size, (1280, 720));
    assert!((small.scale - 1.0).abs() < 1e-6);
}

#[test]
fn letterboxed_content_box_is_respected() {
    let mut r = renderer_with(TWO_LINE_SRT);
    let vp = Viewport {
        width: 1920,
        height: 1080,
        content: Rect {
            x: 0,
            y: 140,
            w: 1920,
            h: 800,
        },
        bottom_inset: 0,
    };
    let f = r.frame(2000, &vp);
    let bot = f
        .masks
        .iter()
        .filter(|m| m.kind == MaskKind::Backdrop)
        .map(|m| m.y + m.h as i32)
        .max()
        .unwrap();
    // Sits inside the picture (bottom 940), 6% of 800 up from it.
    assert!((940 - 52..=940 - 46).contains(&bot), "{bot}");
}

#[test]
fn event_cache_stays_within_budget_over_a_long_track() {
    let mut srt = String::new();
    for i in 0..300 {
        srt.push_str(&format!(
            "{}\n00:{:02}:{:02},000 --> 00:{:02}:{:02},500\nCue number {i} with some text\nand a second line {i}\n\n",
            i + 1,
            i / 60,
            i % 60,
            i / 60,
            i % 60
        ));
    }
    let cap = 200 * 1024;
    let mut r = Renderer::new(Config {
        event_cache_bytes: cap,
        glyph_cache_bytes: 64 * 1024,
    });
    r.add_font(FONT.to_vec()).unwrap();
    r.load(Script::parse(&srt));
    for i in 0..300i64 {
        let f = r.frame(i * 1000 + 200, &vp());
        assert!(f.changed && !f.masks.is_empty());
        let s = r.stats();
        assert!(
            s.event_cache_bytes <= cap,
            "{} > {cap}",
            s.event_cache_bytes
        );
        assert!(s.glyph_cache_bytes <= 64 * 1024 + 16 * 1024);
    }
    assert_eq!(r.stats().events_rastered, 300);
}

#[test]
fn bad_font_bytes_are_rejected_and_no_font_draws_nothing() {
    let mut r = Renderer::default();
    assert!(r.add_font(vec![1, 2, 3]).is_err());
    r.load(Script::parse(TWO_LINE_SRT));
    let f = r.frame(2000, &vp());
    assert!(f.changed && f.masks.is_empty());
}

#[test]
fn bidi_text_draws_in_visual_order() {
    // Hebrew letters are absent from Liberation Sans so draw as .notdef;
    // this checks the RTL path runs and produces a line, not glyph shapes.
    let srt = "1\n00:00:00,000 --> 00:00:09,000\nabc \u{5d0}\u{5d1}\u{5d2} def\n";
    let mut r = renderer_with(srt);
    let f = r.frame(1000, &vp());
    assert_eq!(count(&f, MaskKind::Backdrop), 1);
}
