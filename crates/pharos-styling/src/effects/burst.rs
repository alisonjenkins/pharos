//! Burst (Sfx): the display face, away from the dialogue, a small tilt and a
//! pop. The tilt comes from the line index.

use super::{ev, place_tags, seed};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let s = seed(fx.line);
    let degrees = 3 + (s >> 4) % 4;
    let tilt = if s & 1 == 0 {
        degrees as i64
    } else {
        -(degrees as i64)
    };
    // The style sits at the top; a plan placement overrides it.
    let place = place_tags(fx.placement);
    let pop = "\\fscx130\\fscy130";
    let tags = match tier {
        Tier::Low => format!("{place}\\frz{tilt}\\bord5"),
        Tier::Tv => format!("{place}\\frz{tilt}{pop}\\t(0,140,\\fscx100\\fscy100)"),
        Tier::Desktop => format!(
            "{place}\\frz{tilt}{pop}\\blur3\\t(0,140,\\fscx100\\fscy100)\\t(0,320,\\blur0.8)"
        ),
    };
    vec![ev(
        line,
        fx,
        names::BURST,
        &tags,
        &text::flat_unwrapped(line),
    )]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Placement, Tier};

    #[test]
    fn brackets_are_dropped_and_the_display_face_is_used() {
        for tier in Tier::ALL {
            let evs = run(&line("[BOOM]"), &fx(Intent::Sfx, 1), tier);
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0].style, "Burst");
            assert!(evs[0].text.ends_with("}BOOM"), "{}", evs[0].text);
            assert_tags(&evs[0], &["\\frz"], &[]);
        }
    }

    #[test]
    fn low_is_static() {
        let evs = run(&line("*bang*"), &fx(Intent::Sfx, 1), Tier::Low);
        assert_tags(&evs[0], &["\\bord5"], &["\\t(", "\\blur"]);
    }

    #[test]
    fn tv_pops_once_and_desktop_adds_a_settling_glow() {
        let tv = run(&line("[BOOM]"), &fx(Intent::Sfx, 1), Tier::Tv);
        assert_eq!(tv[0].text.matches("\\t(").count(), 1);
        assert_tags(&tv[0], &["\\fscx130"], &["\\blur"]);
        let d = run(&line("[BOOM]"), &fx(Intent::Sfx, 1), Tier::Desktop);
        assert_eq!(d[0].text.matches("\\t(").count(), 2);
        assert_tags(&d[0], &["\\blur3", "\\t(0,320,\\blur0.8)"], &[]);
    }

    #[test]
    fn tilt_is_small_and_seeded() {
        for i in 0..40 {
            let mut f = fx(Intent::Sfx, 1);
            f.line = i;
            let t = run(&line("[x]"), &f, Tier::Low)[0].text.clone();
            let deg: i64 = t
                .split("\\frz")
                .nth(1)
                .unwrap_or("0")
                .split('\\')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(99);
            assert!((3..=6).contains(&deg.abs()), "{t}");
        }
    }

    #[test]
    fn free_placement_wins_over_the_top_style() {
        let mut f = fx(Intent::Sfx, 1);
        f.placement = Placement::Free { x: 1500, y: 200 };
        let evs = run(&line("[BOOM]"), &f, Tier::Low);
        assert_tags(&evs[0], &["\\an5\\pos(1500,200)"], &[]);
    }
}
