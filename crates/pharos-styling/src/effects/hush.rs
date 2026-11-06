//! Hush (Whisper): smaller, 70% opaque, a soft blur (a border on the TV
//! tier), a slow fade.

use super::{ev, fad, place_tags};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, _theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    // 70% opaque; louder intensities were still whispers, so only slightly.
    let alpha = if fx.intensity >= 2 {
        "\\alpha&H66&"
    } else {
        "\\alpha&H4D&"
    };
    let tags = match tier {
        Tier::Low => format!("{place}{alpha}"),
        Tier::Tv => format!("{place}{alpha}\\bord2{}", fad(line, 400, 400)),
        Tier::Desktop => format!(
            "{place}{alpha}\\blur2\\t(0,700,\\blur0.8){}",
            fad(line, 600, 600)
        ),
    };
    vec![ev(line, fx, names::WHISPER, &tags, &text::flat(line))]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_smaller_and_dim_only() {
        let evs = run(&line("psst"), &fx(Intent::Whisper, 1), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Whisper");
        assert_tags(&evs[0], &["\\alpha&H4D&"], &["\\fad", "\\blur", "\\t("]);
    }

    #[test]
    fn tv_uses_a_border_and_a_fade_not_a_blur() {
        let evs = run(&line("psst"), &fx(Intent::Whisper, 1), Tier::Tv);
        assert_tags(
            &evs[0],
            &["\\bord2", "\\fad(400,400)"],
            &["\\blur", "\\be", "\\t("],
        );
    }

    #[test]
    fn desktop_blurs_softly_and_fades_slowly() {
        let evs = run(&line("psst"), &fx(Intent::Whisper, 1), Tier::Desktop);
        assert_tags(
            &evs[0],
            &["\\blur2", "\\t(0,700,\\blur0.8)", "\\fad(600,600)"],
            &[],
        );
    }

    #[test]
    fn fades_are_limited_to_a_third_of_a_short_line() {
        let mut l = line("psst");
        l.end_ms = l.start_ms + 600;
        let evs = run(&l, &fx(Intent::Whisper, 1), Tier::Tv);
        assert_tags(&evs[0], &["\\fad(200,200)"], &[]);
    }
}
