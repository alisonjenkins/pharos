//! Aside (OffScreen): the dialogue face, dimmer and italic.

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let dim = "\\alpha&H30&";
    let tags = match tier {
        Tier::Low => format!("{place}{dim}"),
        Tier::Tv => format!("{place}{dim}{}", fad(line, 150, 150)),
        Tier::Desktop => format!("{place}{dim}\\blur0.5{}", fad(line, 220, 220)),
    };
    vec![ev(line, fx, names::ASIDE, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn dim_at_every_tier_and_italic_by_style() {
        for tier in Tier::ALL {
            let evs = run(&line("Over here!"), &fx(Intent::OffScreen, 1), tier);
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0].style, "Aside");
            assert_tags(&evs[0], &["\\alpha&H30&"], &["\\t("]);
        }
    }

    #[test]
    fn fades_from_tv_up_and_blurs_only_on_desktop() {
        let low = run(&line("Over here!"), &fx(Intent::OffScreen, 1), Tier::Low);
        assert_tags(&low[0], &[], &["\\fad", "\\blur"]);
        let tv = run(&line("Over here!"), &fx(Intent::OffScreen, 1), Tier::Tv);
        assert_tags(&tv[0], &["\\fad(150,150)"], &["\\blur"]);
        let d = run(
            &line("Over here!"),
            &fx(Intent::OffScreen, 1),
            Tier::Desktop,
        );
        assert_tags(&d[0], &["\\fad(220,220)", "\\blur0.5"], &[]);
    }
}
