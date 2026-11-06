//! Impact (Shout, intensity 1-2): a 115% scale pop settling to 100% over
//! 120 ms, a slight tilt, a brief glow.

use super::{ev, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, rgb, tag_colour, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, theme: &Theme, tier: Tier) -> Vec<Event> {
    let strong = fx.intensity >= 2;
    let bord = if strong { 5 } else { 4 };
    let tilt = if strong { -3 } else { -2 };
    let place = place_tags(fx.placement);
    let pop = format!("\\frz{tilt}\\fscx115\\fscy115");
    let settle = "\\fscx100\\fscy100\\frz0";
    let tags = match tier {
        Tier::Low => format!("{place}\\bord{bord}"),
        Tier::Tv => format!("{place}\\bord{bord}{pop}\\t(0,120,{settle})"),
        Tier::Desktop => {
            let outline = theme
                .style(names::SHOUT)
                .map_or(rgb(42, 10, 0), |s| s.outline_colour);
            format!(
                "{place}\\bord{}\\blur4\\3c{}{pop}\\t(0,120,{settle})\\t(0,260,\\blur1\\bord{bord}\\3c{})",
                bord + 3,
                tag_colour(rgb(255, 176, 64)),
                tag_colour(outline),
            )
        }
    };
    vec![ev(line, fx, names::SHOUT, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_static_and_one_event() {
        let evs = run(&line("Stop!"), &fx(Intent::Shout, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Shout");
        assert_tags(
            &evs[0],
            &["\\bord4"],
            &["\\t(", "\\fscx", "\\blur", "\\move", "\\fad"],
        );
    }

    #[test]
    fn tv_pops_with_a_single_t() {
        let evs = run(&line("Stop!"), &fx(Intent::Shout, 1), Tier::Tv);
        assert_eq!(evs.len(), 1);
        assert_tags(
            &evs[0],
            &[
                "\\fscx115",
                "\\t(0,120,\\fscx100\\fscy100\\frz0)",
                "\\frz-2",
            ],
            &["\\blur", "\\move"],
        );
        assert_eq!(evs[0].text.matches("\\t(").count(), 1);
    }

    #[test]
    fn desktop_adds_a_glow_that_settles() {
        let evs = run(&line("Stop!"), &fx(Intent::Shout, 2), Tier::Desktop);
        assert_eq!(evs.len(), 1);
        assert_tags(
            &evs[0],
            &["\\blur4", "\\t(0,260,\\blur1\\bord5", "\\frz-3", "\\bord8"],
            &[],
        );
        assert_eq!(evs[0].text.matches("\\t(").count(), 2);
    }

    #[test]
    fn placement_is_applied() {
        let mut f = fx(Intent::Shout, 1);
        f.placement = crate::plan::Placement::Top;
        let evs = run(&line("Stop!"), &f, Tier::Tv);
        assert!(evs[0].text.starts_with("{\\an8"));
    }
}
