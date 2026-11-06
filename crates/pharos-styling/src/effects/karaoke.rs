//! Karaoke from the source's own syllable timings: Sweep (`\kf` fill, TV and
//! Desktop), Bounce (Desktop: syllables lift on their beat) and Ember
//! (Desktop: a glow trail behind the fill).

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{Karaoke, LineEffect, Tier};
use crate::source::{SourceLine, Syllable};
use crate::text;
use crate::theme::{names, rgb, tag_colour, Theme};

/// Most syllable groups that get a lift: each costs one `\t`, and Desktop
/// allows four per event.
const MAX_LIFTS: usize = 4;

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let Some(syllables) = line.karaoke.as_deref() else {
        return vec![ev(
            line,
            fx,
            names::KARAOKE,
            &place_tags(fx.placement),
            &text::flat(line),
        )];
    };
    let place = place_tags(fx.placement);
    let base = line.layer.max(0);
    // Each mode needs a tier; anything richer than the tier falls to Sweep.
    let mode = match (fx.karaoke, tier) {
        (_, Tier::Low) => Karaoke::None,
        (Karaoke::Bounce | Karaoke::Ember, Tier::Tv) => Karaoke::Sweep,
        (Karaoke::None, _) => Karaoke::Sweep,
        (m, _) => m,
    };
    match mode {
        // Static: the sung colours are not animated at the low tier.
        Karaoke::None => vec![ev(line, fx, names::KARAOKE, &place, &text::flat(line))],
        Karaoke::Sweep => {
            let lead = format!("{place}{}", fad(line, 150, 150));
            vec![ev(
                line,
                fx,
                names::KARAOKE,
                "",
                &fill(syllables, "kf", &lead, &|_| String::new()),
            )]
        }
        Karaoke::Bounce => bounce(line, fx, syllables, &place),
        Karaoke::Ember => {
            let lead = format!("{place}{}", fad(line, 150, 150));
            // The glow is an outline-only copy filled by \ko, under the text.
            let glow_lead = format!(
                "{place}\\1a&HFF&\\2a&HFF&\\3c{}\\bord8\\blur6{}",
                tag_colour(rgb(255, 150, 60)),
                fad(line, 150, 150)
            );
            vec![
                ev(
                    line,
                    fx,
                    names::KARAOKE,
                    "",
                    &fill(syllables, "ko", &glow_lead, &|_| String::new()),
                )
                .on_layer(base),
                ev(
                    line,
                    fx,
                    names::KARAOKE,
                    "",
                    &fill(syllables, "kf", &lead, &|_| String::new()),
                )
                .on_layer(base + 1),
            ]
        }
    }
}

/// `{\<tag><cs>}text` per syllable; `lead` goes in the first block and
/// `extra(i)` in the block of syllable `i`.
fn fill(syllables: &[Syllable], tag: &str, lead: &str, extra: &dyn Fn(usize) -> String) -> String {
    let mut out = String::new();
    for (i, s) in syllables.iter().enumerate() {
        let head = if i == 0 { lead } else { "" };
        out.push_str(&format!(
            "{{{head}\\{tag}{}{}}}",
            s.dur_ms.div_ceil(10),
            extra(i)
        ));
        out.push_str(&text::esc(&s.text));
    }
    out
}

/// A plain `\k` fill; consecutive syllables are split into up to four groups
/// and each group lifts once, on the beat of its first syllable.
fn bounce(line: &SourceLine, fx: &LineEffect, syllables: &[Syllable], place: &str) -> Vec<Event> {
    let mut beats = Vec::new(); // (index, start ms) of each syllable with text
    let mut t = 0u32;
    for (i, s) in syllables.iter().enumerate() {
        if !s.text.trim().is_empty() {
            beats.push((i, t));
        }
        t = t.saturating_add(s.dur_ms);
    }
    let groups = beats.len().min(MAX_LIFTS);
    let mut lift_at: Vec<(usize, u32)> = Vec::new();
    for g in 0..groups {
        lift_at.push(beats[g * beats.len() / groups]);
    }
    let lead = format!("{place}{}", fad(line, 150, 150));
    let extra = |i: usize| {
        lift_at
            .iter()
            .find(|&&(at, _)| at == i)
            .map(|&(_, start)| format!("\\fscy100\\t({start},{},\\fscy112)", start + 110))
            .unwrap_or_default()
    };
    vec![ev(
        line,
        fx,
        names::KARAOKE,
        "",
        &fill(syllables, "k", &lead, &extra),
    )]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::plan::Intent;

    fn song() -> SourceLine {
        let mut l = line("ka ra o ke");
        l.karaoke = Some(
            [(250, "ka "), (500, "ra "), (100, "o "), (50, "ke")]
                .iter()
                .map(|&(d, t)| Syllable {
                    dur_ms: d,
                    text: t.to_owned(),
                })
                .collect(),
        );
        l
    }

    fn sing(mode: Karaoke, tier: Tier) -> Vec<Event> {
        let mut f = fx(Intent::Plain, 1);
        f.karaoke = mode;
        run(&song(), &f, tier)
    }

    #[test]
    fn low_is_static_text_in_the_karaoke_style() {
        let evs = sing(Karaoke::Sweep, Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Karaoke");
        assert_eq!(evs[0].text, "ka ra o ke");
    }

    #[test]
    fn sweep_uses_kf_with_the_source_durations() {
        for tier in [Tier::Tv, Tier::Desktop] {
            let evs = sing(Karaoke::Sweep, tier);
            assert_eq!(evs.len(), 1);
            assert_tags(
                &evs[0],
                &[
                    "\\kf25}ka ",
                    "\\kf50}ra ",
                    "\\kf10}o ",
                    "\\kf5}ke",
                    "\\fad(150,150)",
                ],
                &["\\k25", "\\t("],
            );
        }
    }

    #[test]
    fn bounce_and_ember_fall_back_to_sweep_on_tv() {
        for mode in [Karaoke::Bounce, Karaoke::Ember] {
            let evs = sing(mode, Tier::Tv);
            assert_eq!(evs.len(), 1);
            assert_tags(&evs[0], &["\\kf25}"], &["\\t(", "\\ko"]);
        }
    }

    #[test]
    fn bounce_lifts_each_group_once_on_its_beat() {
        let evs = sing(Karaoke::Bounce, Tier::Desktop);
        assert_eq!(evs.len(), 1);
        assert_tags(
            &evs[0],
            &[
                "\\k25",
                "\\t(0,110,\\fscy112)",
                "\\t(250,360,\\fscy112)",
                "\\t(750,860,\\fscy112)",
                "\\t(850,960,\\fscy112)",
            ],
            &["\\kf"],
        );
        assert_eq!(evs[0].text.matches("\\t(").count(), 4);
    }

    #[test]
    fn bounce_never_exceeds_four_lifts() {
        let mut l = line("x");
        l.karaoke = Some(
            (0..30)
                .map(|_| Syllable {
                    dur_ms: 100,
                    text: "la".into(),
                })
                .collect(),
        );
        let mut f = fx(Intent::Plain, 1);
        f.karaoke = Karaoke::Bounce;
        let evs = run(&l, &f, Tier::Desktop);
        assert_eq!(evs[0].text.matches("\\t(").count(), 4);
    }

    #[test]
    fn ember_adds_a_glow_trail_under_the_fill() {
        let evs = sing(Karaoke::Ember, Tier::Desktop);
        assert_eq!(evs.len(), 2);
        assert!(evs[0].layer < evs[1].layer);
        assert_tags(&evs[0], &["\\ko25}", "\\blur6"], &["\\kf"]);
        assert_tags(&evs[1], &["\\kf25}"], &["\\blur"]);
    }
}
