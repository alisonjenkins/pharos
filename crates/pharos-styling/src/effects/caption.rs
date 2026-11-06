//! Caption (Narration): the condensed face in a box at the top of the frame.

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let tags = match tier {
        Tier::Low => place,
        Tier::Tv => format!("{place}{}", fad(line, 200, 200)),
        Tier::Desktop => format!(
            "{place}\\fscx94\\fscy94\\t(0,260,\\fscx100\\fscy100){}",
            fad(line, 260, 260)
        ),
    };
    vec![ev(line, fx, names::CAPTION, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Placement, Tier};

    #[test]
    fn low_is_the_top_box_style_only() {
        let evs = run(&line("Long ago..."), &fx(Intent::Narration, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Caption");
        assert_eq!(evs[0].text, "Long ago...");
    }

    #[test]
    fn tv_fades_only() {
        let evs = run(&line("Long ago..."), &fx(Intent::Narration, 1), Tier::Tv);
        assert_tags(&evs[0], &["\\fad(200,200)"], &["\\t(", "\\blur"]);
    }

    #[test]
    fn desktop_settles_into_place() {
        let evs = run(
            &line("Long ago..."),
            &fx(Intent::Narration, 1),
            Tier::Desktop,
        );
        assert_tags(
            &evs[0],
            &[
                "\\fscx94",
                "\\t(0,260,\\fscx100\\fscy100)",
                "\\fad(260,260)",
            ],
            &[],
        );
    }

    #[test]
    fn a_free_placement_overrides_the_top() {
        let mut f = fx(Intent::Narration, 1);
        f.placement = Placement::Free { x: 10, y: 20 };
        let evs = run(&line("Long ago..."), &f, Tier::Low);
        assert_tags(&evs[0], &["\\an5\\pos(10,20)"], &[]);
    }
}
