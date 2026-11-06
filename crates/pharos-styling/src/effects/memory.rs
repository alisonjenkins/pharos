//! Memory (Flashback): desaturated, a warm tint, letter-spaced.

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let faded = if fx.intensity >= 2 {
        "\\alpha&H20&"
    } else {
        ""
    };
    let tags = match tier {
        Tier::Low => format!("{place}\\fsp3{faded}"),
        Tier::Tv => format!("{place}\\fsp3{faded}{}", fad(line, 300, 300)),
        // The letters drift apart as the memory surfaces.
        Tier::Desktop => format!(
            "{place}\\fsp1\\blur0.5{faded}\\t(0,600,\\fsp4){}",
            fad(line, 450, 450)
        ),
    };
    vec![ev(line, fx, names::MEMORY, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_warm_letter_spaced_and_static() {
        let evs = run(&line("Back then"), &fx(Intent::Flashback, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Memory");
        assert_tags(&evs[0], &["\\fsp3"], &["\\t(", "\\fad", "\\blur"]);
    }

    #[test]
    fn tv_fades() {
        let evs = run(&line("Back then"), &fx(Intent::Flashback, 1), Tier::Tv);
        assert_tags(&evs[0], &["\\fsp3", "\\fad(300,300)"], &["\\t(", "\\blur"]);
    }

    #[test]
    fn desktop_spreads_the_letters_in() {
        let evs = run(&line("Back then"), &fx(Intent::Flashback, 2), Tier::Desktop);
        assert_tags(
            &evs[0],
            &["\\fsp1", "\\t(0,600,\\fsp4)", "\\alpha&H20&"],
            &[],
        );
    }
}
