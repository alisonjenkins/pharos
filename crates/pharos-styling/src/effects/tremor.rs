//! Tremor (Shout, intensity 3, a scream): a few pixels of jitter damped over
//! the line. The offsets and the direction come from the line index.

use super::{anchor, ev, place_tags, seed};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, theme: &Theme, tier: Tier) -> Vec<Event> {
    let place = place_tags(fx.placement);
    let tags = match tier {
        Tier::Low => format!("{place}\\bord6"),
        // One pop: the budget has room for a single \t.
        Tier::Tv => {
            format!("{place}\\bord6\\frz-3\\fscx125\\fscy125\\t(0,150,\\fscx100\\fscy100\\frz0)")
        }
        Tier::Desktop => desktop(fx, theme),
    };
    vec![ev(line, fx, names::SCREAM, &tags, &text::flat(line))]
}

/// A start offset easing to the resting point with one `\move`, and a
/// rotation shake in four damped `\t` steps.
fn desktop(fx: &LineEffect, theme: &Theme) -> String {
    let s = seed(fx.line);
    let sign = |bit: u64| if (s >> bit) & 1 == 0 { 1 } else { -1 };
    let dx = (4 + (s >> 8) % 5) as i32 * sign(0);
    let dy = (3 + (s >> 16) % 4) as i32 * sign(1);
    let amp = if sign(2) > 0 { 1.0 } else { -1.0 };
    let (an, x, y) = anchor(theme, names::SCREAM, fx.placement);
    let shake = |k: f64| format!("{:.1}", amp * k);
    format!(
        "\\an{an}\\move({},{},{x},{y},0,260)\\bord6\\blur0.8\\frz{}\\t(0,60,\\frz{})\\t(60,120,\\frz{})\\t(120,180,\\frz{})\\t(180,240,\\frz0)",
        x + dx,
        y + dy,
        shake(2.4),
        shake(-1.8),
        shake(1.2),
        shake(-0.6),
    )
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Placement, Tier};

    #[test]
    fn low_is_a_static_bold_larger_scream() {
        let evs = run(&line("AAAH!"), &fx(Intent::Shout, 3), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Scream");
        assert_tags(&evs[0], &["\\bord6"], &["\\t(", "\\move", "\\blur"]);
    }

    #[test]
    fn tv_is_a_single_pop() {
        let evs = run(&line("AAAH!"), &fx(Intent::Shout, 3), Tier::Tv);
        assert_eq!(evs[0].text.matches("\\t(").count(), 1);
        assert_tags(&evs[0], &["\\fscx125"], &["\\move", "\\blur"]);
    }

    #[test]
    fn desktop_jitters_with_one_move_and_four_damped_steps() {
        let evs = run(&line("AAAH!"), &fx(Intent::Shout, 3), Tier::Desktop);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].text.matches("\\t(").count(), 4);
        assert_eq!(evs[0].text.matches("\\move(").count(), 1);
        assert_tags(&evs[0], &["\\an2", "\\frz"], &[]);
    }

    #[test]
    fn jitter_is_seeded_from_the_line_index() {
        let a = run(&line("AAAH!"), &fx(Intent::Shout, 3), Tier::Desktop);
        let b = run(&line("AAAH!"), &fx(Intent::Shout, 3), Tier::Desktop);
        assert_eq!(a, b);
        let mut other = fx(Intent::Shout, 3);
        other.line = 8;
        assert_ne!(a, run(&line("AAAH!"), &other, Tier::Desktop));
    }

    #[test]
    fn free_placement_moves_around_the_point() {
        let mut f = fx(Intent::Shout, 3);
        f.placement = Placement::Free { x: 500, y: 300 };
        let evs = run(&line("AAAH!"), &f, Tier::Desktop);
        assert_tags(&evs[0], &["\\an5", ",500,300,0,260)"], &["\\pos"]);
    }
}
