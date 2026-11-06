//! V533: random plans over random sources compile to ASS that the renderer's
//! parser reads with no bad lines, that stays inside its tier's budget, and
//! whose events never leave their source line's time range. Passthrough
//! events (the source's own signs) are exempt from the budget, as documented
//! on `budget::check`; the generated sources stay under 6 starts a second, so
//! the dense-source exception does not arise.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use pharos_styling::budget;
use pharos_styling::plan::{
    EffectPlan, EffectsLevel, Intent, Karaoke, LineEffect, Placement, Tier,
};
use pharos_styling::source::{self, SourceLine};
use pharos_styling::theme::Theme;
use pharos_styling::{compile, DIRECTOR_VERSION};
use proptest::prelude::*;

/// `PROPTEST_CASES` override (V533 runs 512 in `just test-thorough`).
pub fn cfg() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Container {
    Srt,
    Vtt,
    Ass,
}

#[derive(Clone, Debug)]
struct GenLine {
    /// Start-to-start gap and duration in 10 ms steps. The gap floor keeps the
    /// source itself under 6 starts per second.
    gap: i64,
    dur: i64,
    text: usize,
    markup: u8,
    /// A source sign (typeset): 0 plain tags, 1 with `\t`/`\blur`, 2 legacy `\a6`.
    sign: Option<u8>,
    effect: Option<GenEffect>,
}

#[derive(Clone, Debug)]
struct GenEffect {
    intent: usize,
    intensity: u8,
    placement: u8,
    karaoke: u8,
}

const TEXTS: &[&str] = &[
    "Hello there",
    "(thinking out loud)",
    "[BOOM]",
    "*bang*",
    "braces {a} and } { odd",
    "back\\slash \\N inside",
    "first line\nsecond line",
    "日本語のテキスト",
    "it's <3 & more > less",
    "WHAT?! NO WAY!",
    "well, well, well, what now,",
    "",
];

fn gen_effect() -> impl Strategy<Value = GenEffect> {
    (0..Intent::ALL.len(), 0u8..=3, 0u8..3, 0u8..4).prop_map(
        |(intent, intensity, placement, karaoke)| GenEffect {
            intent,
            intensity,
            placement,
            karaoke,
        },
    )
}

fn gen_line() -> impl Strategy<Value = GenLine> {
    (
        // Mostly dense (17-40 steps), sometimes sparse.
        prop_oneof![3 => 17i64..40, 1 => 40i64..300],
        20i64..600,
        0..TEXTS.len(),
        0u8..4,
        prop::option::weighted(0.08, 0u8..3),
        prop::option::weighted(0.85, gen_effect()),
    )
        .prop_map(|(gap, dur, text, markup, sign, effect)| GenLine {
            gap,
            dur,
            text,
            markup,
            sign,
            effect,
        })
}

fn clock(ms: i64, container: Container) -> String {
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    match container {
        Container::Srt => format!("{h:02}:{m:02}:{s:02},{frac:03}"),
        Container::Vtt => format!("{h:02}:{m:02}:{s:02}.{frac:03}"),
        Container::Ass => format!("{h}:{m:02}:{s:02}.{:02}", frac / 10),
    }
}

/// Marks the text up: none, `<i>`/`{\i1}`, karaoke (ASS), partial italics.
fn markup(text: &str, kind: u8, container: Container) -> String {
    match (container, kind) {
        (Container::Ass, 0) => text.replace('\n', "\\N"),
        (Container::Ass, 1) => format!("{{\\i1}}{}", text.replace('\n', "\\N")),
        (Container::Ass, 2) => "{\\k20}ka {\\kf30}ra {\\ko10}o {\\K15}ke".to_owned(),
        (Container::Ass, _) => format!("{{\\fax1\\q2}}{}{{\\i1}} tail", text.replace('\n', "\\N")),
        (_, 0) => text.to_owned(),
        (_, 1) => format!("<i>{text}</i>"),
        (_, 2) => format!("{{\\i1}}{text}{{\\i0}}"),
        (_, _) => format!("<b>{text}</b> <i>tail</i>"),
    }
}

/// The source script and the times each line occupies in it.
fn build(container: Container, gen: &[GenLine], offset_ms: i64) -> String {
    let mut out = match container {
        Container::Srt => String::new(),
        Container::Vtt => "WEBVTT\n\n".to_owned(),
        Container::Ass => "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n".to_owned(),
    };
    let mut start = 1000;
    for (i, g) in gen.iter().enumerate() {
        start += g.gap * 10;
        let off = if matches!(container, Container::Ass) {
            0
        } else {
            offset_ms
        };
        let (a, b) = (start + off, start + off + g.dur * 10);
        let (a, b) = (clock(a, container), clock(b, container));
        match container {
            Container::Ass => {
                let text = match g.sign {
                    Some(0) => "{\\pos(300,200)\\frz4\\clip(0,0,900,500)}Sign text".to_owned(),
                    Some(1) => {
                        "{\\pos(3,2)\\t(0,1,\\fax1)\\blur3\\t(1,2,\\fscx90)}Busy sign".to_owned()
                    }
                    Some(_) => "{\\a6}Legacy top".to_owned(),
                    None => markup(TEXTS[g.text], g.markup, container),
                };
                out.push_str(&format!("Dialogue: 0,{a},{b},Default,,0,0,0,,{text}\n"));
            }
            _ => {
                out.push_str(&format!(
                    "{}\n{a} --> {b}\n{}\n\n",
                    i + 1,
                    markup(TEXTS[g.text], g.markup, container)
                ));
            }
        }
    }
    out
}

fn plan_of(gen: &[GenLine], cap_intensity: Option<u8>) -> EffectPlan {
    EffectPlan {
        director_version: DIRECTOR_VERSION,
        lines: gen
            .iter()
            .enumerate()
            .filter_map(|(line, g)| {
                let e = g.effect.as_ref()?;
                Some(LineEffect {
                    line,
                    intent: Intent::ALL[e.intent],
                    intensity: cap_intensity.map_or(e.intensity, |c| e.intensity.min(c)),
                    placement: match e.placement {
                        0 => Placement::Default,
                        1 => Placement::Top,
                        _ => Placement::Free { x: 400, y: 300 },
                    },
                    karaoke: match e.karaoke {
                        0 => Karaoke::None,
                        1 => Karaoke::Sweep,
                        2 => Karaoke::Bounce,
                        _ => Karaoke::Ember,
                    },
                    reason: String::new(),
                    static_only: false,
                })
            })
            .collect(),
    }
}

/// `H:MM:SS.cc` to ms.
fn ass_ms(s: &str) -> i64 {
    let p: Vec<&str> = s.split([':', '.']).collect();
    ((p[0].parse::<i64>().unwrap() * 60 + p[1].parse::<i64>().unwrap()) * 60
        + p[2].parse::<i64>().unwrap())
        * 1000
        + p[3].parse::<i64>().unwrap() * 10
}

fn check_output(ass: &str, lines: &[SourceLine], tier: Tier) -> Result<(), TestCaseError> {
    let script = pharos_subs::Script::parse_bytes(ass.as_bytes(), None);
    prop_assert_eq!(script.stats.bad_lines, 0, "{}", ass);
    if let Err(b) = budget::check(ass, tier) {
        return Err(TestCaseError::fail(format!("{b:?}\n{ass}")));
    }
    let mut per_line: BTreeMap<usize, usize> = BTreeMap::new();
    for d in ass.lines().filter(|l| l.starts_with("Dialogue:")) {
        let f: Vec<&str> = d.splitn(10, ',').collect();
        let idx: usize = f[4]
            .trim_start_matches(budget::PASSTHROUGH_PREFIX)
            .parse()
            .unwrap();
        let (a, b) = (ass_ms(f[1]), ass_ms(f[2]));
        let src = &lines[idx];
        prop_assert!(
            a >= src.start_ms && b <= src.end_ms && a < b,
            "event {a}-{b} outside source {}-{}: {d}",
            src.start_ms,
            src.end_ms
        );
        *per_line.entry(idx).or_default() += 1;
    }
    for (idx, n) in per_line {
        prop_assert!(
            n <= budget::max_events_per_line(tier),
            "line {idx} became {n} events at {tier:?}"
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn compiled_output_parses_and_keeps_budgets(
        gen in prop::collection::vec(gen_line(), 1..28),
        container in prop_oneof![Just(Container::Srt), Just(Container::Vtt), Just(Container::Ass)],
        offset in 0i64..10,
    ) {
        let script = build(container, &gen, offset);
        let lines = source::read(&script).unwrap().lines;
        prop_assert_eq!(lines.len(), gen.len());
        let plan = plan_of(&gen, None);
        let theme = Theme::pharos();
        for tier in Tier::ALL {
            for level in [EffectsLevel::Subtle, EffectsLevel::Full] {
                let ass = compile(&plan, &lines, &theme, tier, level);
                check_output(&ass, &lines, tier)?;
                if level == EffectsLevel::Subtle {
                    // Desktop-only forms of intensity 2 and 3 never appear.
                    for marker in ["Scream", "\\fs130", "\\alpha&H66&", "\\alpha&H20&"] {
                        prop_assert!(!ass.contains(marker), "{marker} at Subtle:\n{ass}");
                    }
                }
                // Deterministic.
                prop_assert_eq!(&ass, &compile(&plan, &lines, &theme, tier, level));
            }
        }
    }

    #[test]
    fn subtle_caps_intensity_at_one_and_off_is_plain(
        gen in prop::collection::vec(gen_line(), 1..28),
        container in prop_oneof![Just(Container::Srt), Just(Container::Ass)],
    ) {
        let script = build(container, &gen, 0);
        let lines = source::read(&script).unwrap().lines;
        let theme = Theme::pharos();
        let full = plan_of(&gen, None);
        let capped = plan_of(&gen, Some(1));
        let empty = EffectPlan { director_version: DIRECTOR_VERSION, lines: vec![] };
        for tier in Tier::ALL {
            prop_assert_eq!(
                compile(&full, &lines, &theme, tier, EffectsLevel::Subtle),
                compile(&capped, &lines, &theme, tier, EffectsLevel::Subtle)
            );
            prop_assert_eq!(
                compile(&full, &lines, &theme, tier, EffectsLevel::Off),
                compile(&empty, &lines, &theme, tier, EffectsLevel::Off)
            );
        }
    }
}
