//! Property test: the director never panics and always answers every line (V535).

use pharos_styling::direct;
use pharos_styling::signals::{LineSignals, Segment, SegmentKind, TrackSignals};
use pharos_styling::source::{SourceFormat, SourceLine, Syllable};
use proptest::prelude::*;

use super::compile_props::cfg;

const TEXTS: [&str; 12] = [
    "Hello there",
    "STOP RIGHT NOW!",
    "[explosion]",
    "*bang*",
    "(whispering) hush",
    "(What is that)",
    "NARRATOR: long ago",
    "(over radio) come in",
    "Fire!",
    "",
    "  ",
    "ÄÖÜ ß! 日本語",
];

fn opt() -> impl Strategy<Value = Option<f32>> {
    prop_oneof![
        Just(None),
        Just(Some(f32::NAN)),
        Just(Some(f32::INFINITY)),
        (-130.0f32..20.0).prop_map(Some),
    ]
}

fn sig() -> impl Strategy<Value = LineSignals> {
    (opt(), opt(), opt(), 0.0f32..1.0, opt()).prop_map(|(l, p, f, v, h)| LineSignals {
        loudness_lufs: l,
        peak_dbfs: p,
        pitch_hz: f,
        voiced_ratio: v,
        high_band_share: h,
    })
}

fn src() -> impl Strategy<Value = SourceLine> {
    (
        0usize..TEXTS.len(),
        0i64..100_000,
        0i64..10_000,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(t, start, len, italic, typeset, karaoke)| SourceLine {
            start_ms: start,
            end_ms: start + len,
            text: TEXTS[t].to_owned(),
            raw: TEXTS[t].to_owned(),
            format: SourceFormat::Srt,
            style: "Default".into(),
            layer: 0,
            italic,
            typeset,
            karaoke: karaoke.then(|| {
                vec![Syllable {
                    dur_ms: 100,
                    text: "la".into(),
                }]
            }),
            style_def: None,
        })
}

fn seg() -> impl Strategy<Value = Segment> {
    (0usize..4, 0u64..100_000, 0u64..50_000).prop_map(|(k, s, l)| Segment {
        kind: [
            SegmentKind::Intro,
            SegmentKind::Outro,
            SegmentKind::Recap,
            SegmentKind::Other,
        ][k],
        start_ms: s,
        end_ms: s + l,
    })
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn any_input_gets_one_reasoned_entry_per_line(
        lines in prop::collection::vec(src(), 0..30),
        sigs in prop::collection::vec(sig(), 0..35),
        cuts in prop::collection::vec(0u64..110_000, 0..6),
        segs in prop::collection::vec(seg(), 0..4),
    ) {
        let d = direct(&lines, &TrackSignals { lines: sigs, cuts_ms: cuts.clone(), bottom_text: vec![] }, &segs);
        prop_assert_eq!(d.plan.lines.len(), lines.len());
        prop_assert_eq!(d.counts.values().sum::<usize>(), lines.len());
        for (i, fx) in d.plan.lines.iter().enumerate() {
            prop_assert_eq!(fx.line, i);
            prop_assert!(!fx.reason.is_empty());
            let l = &lines[i];
            let spans = cuts.iter().any(|&c| (l.start_ms as u64) < c && c < l.end_ms as u64);
            let animated = fx.intent != pharos_styling::Intent::Plain
                || fx.karaoke != pharos_styling::Karaoke::None;
            prop_assert_eq!(fx.static_only, spans && animated);
            if l.typeset {
                prop_assert_eq!(fx.intent, pharos_styling::Intent::Plain);
            }
        }
    }
}
