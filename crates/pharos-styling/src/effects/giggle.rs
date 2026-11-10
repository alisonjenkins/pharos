//! Giggle (Laugh): the rounded hand face in warm pink that bounces as it
//! lands, while small "ha"s bubble up off its ends and fade (B1205): one on
//! TV, two on Desktop, the second a beat later.

use super::{anchor, ev, fad};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, rgb, tag_colour, Theme};

/// What a bubble says.
const BUBBLE: &str = "ha";
/// A bubble's size, how far it rises and drifts outwards, in script units,
/// and how long it lives.
const BUBBLE_FS: u32 = 54;
const RISE: i32 = 150;
const DRIFT: i32 = 50;
const RISE_MS: i64 = 900;
/// The second Desktop bubble leaves this much later.
const STAGGER_MS: i64 = 220;
/// Roughly half a glyph's advance in the hand face, per point of size: a
/// line's half width is its length times this times the style size.
const HALF_ADVANCE_PER_PT: f32 = 0.23;

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, theme: &Theme, tier: Tier) -> Vec<Event> {
    let words = text::flat_unwrapped(line);
    let base = line.layer.max(0);
    let (an, x, y) = anchor(theme, names::GIGGLE, fx.placement);
    let size = theme.style(names::GIGGLE).map_or(76, |s| s.size) as f32;
    let half = (words.chars().count() as f32 * HALF_ADVANCE_PER_PT * size) as i32;
    let fs = (BUBBLE_FS as f32 * theme.tag_scale).round() as u32;
    let at = format!("\\an{an}\\pos({x},{y})");
    // A bubble leaves the line's left (`side` -1) or right (1) end, a little
    // above its middle, and floats up and outwards, tilting as it goes.
    let bubble = |side: i32, delay: i64, extra: &str| {
        let colour = tag_colour(rgb(255, 170, 210));
        let (x0, y0) = (x + side * (half + 12), y - (size * 0.6) as i32);
        let tilt = side * 12;
        let tags = format!(
            "\\an5\\move({x0},{y0},{},{},0,{RISE_MS})\\fs{fs}\\1c{colour}\\alpha&H30&\\frz{}{extra}{}",
            x0 + side * DRIFT,
            y0 - RISE,
            -tilt / 2,
            fad(line, 0, RISE_MS / 2),
        );
        let mut e = ev(line, fx, names::GIGGLE, &tags, BUBBLE).on_layer(base);
        e.start_ms += delay;
        e.end_ms = e.end_ms.min(e.start_ms + RISE_MS);
        e
    };
    let main = |tags: String| ev(line, fx, names::GIGGLE, &tags, &words).on_layer(base + 1);
    let mut out = match tier {
        Tier::Low => vec![ev(
            line,
            fx,
            names::GIGGLE,
            &super::place_tags(fx.placement),
            &words,
        )],
        Tier::Tv => vec![
            bubble(1, 0, ""),
            main(format!(
                "{at}\\fscx125\\fscy125\\frz-6\\t(0,220,\\fscx100\\fscy100\\frz0)"
            )),
        ],
        // A bounce that settles: squash, tip one way, the other, then rest.
        Tier::Desktop => vec![
            bubble(1, 0, "\\t(0,900,\\frz-12)"),
            bubble(-1, STAGGER_MS, "\\t(0,900,\\frz12)"),
            main(format!(
                "{at}\\fscx125\\fscy125\\t(0,140,\\fscx100\\fscy92\\frz-6)\\t(140,280,\\fscy110\\frz6)\\t(280,420,\\fscy96\\frz-3)\\t(420,560,\\fscy100\\frz0)"
            )),
        ],
    };
    out.retain(Event::is_visible);
    out
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::budget;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_one_still_event_without_its_brackets() {
        let evs = run(&line("(laughs)"), &fx(Intent::Laugh, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Giggle");
        assert_eq!(evs[0].text, "laughs");
    }

    #[test]
    fn tv_pops_once_with_a_rising_bubble_behind() {
        let evs = run(&line("(laughs)"), &fx(Intent::Laugh, 1), Tier::Tv);
        assert_eq!(evs.len(), 2);
        assert_tags(
            &evs[0],
            &["\\move(", "\\fad(0,", "\\fs54"],
            &["\\blur", "\\t("],
        );
        assert!(evs[0].text.ends_with("}ha"), "{}", evs[0].text);
        assert_tags(&evs[1], &["\\t(0,220,"], &["\\blur", "\\move"]);
        assert!(evs[0].layer < evs[1].layer);
        assert!(
            evs[0].end_ms <= evs[0].start_ms + super::RISE_MS,
            "the bubble is brief"
        );
        assert!(evs[1].text.ends_with("}laughs"), "{}", evs[1].text);
    }

    #[test]
    fn desktop_bounces_within_its_budget() {
        let evs = run(&line("[giggling]"), &fx(Intent::Laugh, 1), Tier::Desktop);
        assert_eq!(evs.len(), 3);
        assert!(evs.len() <= budget::max_events_per_line(Tier::Desktop));
        assert_eq!(evs[2].text.matches("\\t(").count(), 4);
        for b in &evs[..2] {
            assert_tags(b, &["\\move(", "\\t("], &[]);
            assert!(b.layer < evs[2].layer);
            assert!(b.end_ms <= b.start_ms + super::RISE_MS);
        }
        assert_eq!(evs[1].start_ms, evs[0].start_ms + super::STAGGER_MS);
    }
}
