//! Wire (Phone, radio): boxed, monospaced, with a slight flicker on Desktop.

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
        Tier::Tv => format!("{place}{}", fad(line, 120, 120)),
        // A short stutter of the whole line as the signal comes in.
        Tier::Desktop => format!(
            "{place}\\alpha&H40&\\t(0,60,\\alpha&H00&)\\t(60,120,\\alpha&H50&)\\t(120,200,\\alpha&H00&)"
        ),
    };
    vec![ev(line, fx, names::WIRE, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_the_boxed_mono_style_only() {
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

    #[test]
    fn only_desktop_flickers() {
        let d = run(&line("Moshi moshi"), &fx(Intent::Phone, 1), Tier::Desktop);
        assert_eq!(d[0].text.matches("\\t(").count(), 3);
        assert_tags(&d[0], &["\\alpha&H40&"], &["\\fad"]);
        let tv = run(&line("Moshi moshi"), &fx(Intent::Phone, 1), Tier::Tv);
        assert_tags(&tv[0], &[], &["\\t(", "\\alpha"]);
    }
}
