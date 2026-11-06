//! Inner voice (Thought): italic in the handwritten face, a soft halo, and
//! the source's wrapping parentheses dropped.

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, rgb, tag_colour, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let words = text::flat_unwrapped(line);
    let halo = tag_colour(rgb(150, 130, 230));
    match tier {
        Tier::Low => vec![ev(line, fx, names::THOUGHT, &place, &words)],
        Tier::Tv => {
            let tags = format!("{place}\\bord4{}", fad(line, 300, 300));
            vec![ev(line, fx, names::THOUGHT, &tags, &words)]
        }
        Tier::Desktop => {
            // The halo is a blurred outline-only copy drawn under the text.
            let under = format!(
                "{place}\\1a&HFF&\\3c{halo}\\3a&H50&\\bord9\\blur7{}",
                fad(line, 400, 400)
            );
            let over = format!("{place}\\bord3{}", fad(line, 400, 400));
            vec![
                ev(line, fx, names::THOUGHT, &under, &words).on_layer(line.layer.max(0)),
                ev(line, fx, names::THOUGHT, &over, &words).on_layer(line.layer.max(0) + 1),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn brackets_are_dropped_at_every_tier() {
        for tier in Tier::ALL {
            let evs = run(&line("(I should run.)"), &fx(Intent::Thought, 1), tier);
            assert!(
                evs.iter().all(|e| e.text.ends_with("I should run.")),
                "{evs:?}"
            );
            assert_eq!(evs[0].style, "Thought");
        }
    }

    #[test]
    fn low_is_static() {
        let evs = run(&line("(hm)"), &fx(Intent::Thought, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].text, "hm");
    }

    #[test]
    fn tv_has_a_border_and_a_fade() {
        let evs = run(&line("(hm)"), &fx(Intent::Thought, 1), Tier::Tv);
        assert_eq!(evs.len(), 1);
        assert_tags(&evs[0], &["\\bord4", "\\fad(300,300)"], &["\\blur", "\\t("]);
    }

    #[test]
    fn desktop_draws_a_blurred_halo_under_the_text() {
        let evs = run(&line("(hm)"), &fx(Intent::Thought, 1), Tier::Desktop);
        assert_eq!(evs.len(), 2);
        assert!(evs[0].layer < evs[1].layer);
        assert_tags(&evs[0], &["\\blur7", "\\1a&HFF&"], &[]);
        assert_tags(&evs[1], &["\\bord3"], &["\\blur"]);
    }
}
