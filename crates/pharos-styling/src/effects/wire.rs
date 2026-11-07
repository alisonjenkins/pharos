//! Wire (Phone, radio): the dialogue face in silver-blue. Desktop fades the
//! line in from a paler tint, once; TV fades; Low is the style alone (B662).

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, tag_colour, Theme, WIRE_PRIMARY};

/// The tint a Desktop line arrives in before it settles to Wire's colour.
const ARRIVE: u32 = crate::theme::rgb(236, 244, 255);
/// How long the tint takes to settle, in ms.
const SETTLE_MS: i64 = 400;

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let tags = match tier {
        Tier::Low => place,
        Tier::Tv => format!("{place}{}", fad(line, 120, 120)),
        // One fade and one settle of the tint: nothing pulses or repeats.
        Tier::Desktop => format!(
            "{place}{}\\1c{}\\t(0,{},\\1c{})",
            fad(line, 200, 160),
            tag_colour(ARRIVE),
            SETTLE_MS.min((line.end_ms - line.start_ms).max(0)),
            tag_colour(WIRE_PRIMARY),
        ),
    };
    vec![ev(line, fx, names::WIRE, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_the_wire_style_only() {
        let evs = run(&line("Moshi moshi"), &fx(Intent::Phone, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Wire");
        assert_eq!(evs[0].text, "Moshi moshi");
    }

    #[test]
    fn tv_fades_in_and_out() {
        let evs = run(&line("Moshi moshi"), &fx(Intent::Phone, 1), Tier::Tv);
        assert_tags(&evs[0], &["\\fad(120,120)"], &["\\t(", "\\alpha"]);
    }

    /// B662: Desktop fades in once and settles its tint once. The old
    /// three-step alpha stutter read as flicker.
    #[test]
    fn desktop_fades_once_without_a_stutter() {
        let d = run(&line("Moshi moshi"), &fx(Intent::Phone, 1), Tier::Desktop);
        assert_eq!(d[0].text.matches("\\t(").count(), 1);
        assert_tags(
            &d[0],
            &[
                "\\fad(200,160)",
                "\\1c&HFFF4EC&",
                "\\t(0,400,\\1c&HFFE1C8&)",
            ],
            &["\\alpha"],
        );
    }
}
