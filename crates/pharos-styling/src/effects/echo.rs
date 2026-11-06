//! Echo (AttackCall): the line with ghost copies that trail and fade. At
//! intensity 3 on Desktop it becomes a big title-card variant.

use super::{anchor, ev, fad};
use crate::event::Event;
use crate::plan::{LineEffect, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, rgb, tag_colour, Theme};

pub(crate) fn events(line: &SourceLine, fx: &LineEffect, theme: &Theme, tier: Tier) -> Vec<Event> {
    let words = text::flat(line);
    let base = line.layer.max(0);
    let title = tier == Tier::Desktop && fx.intensity >= 3;
    let (an, x, y) = if title {
        (5, theme.play_res_x / 2, theme.play_res_y * 2 / 5)
    } else {
        anchor(theme, names::ECHO, fx.placement)
    };
    let size = if title { "\\fs130" } else { "" };
    let ghost_colour = tag_colour(rgb(120, 170, 255));
    // A ghost drifts down and to the right while it fades out.
    let ghost = |k: i32, alpha: &str, blur: &str| {
        let (dx, dy) = (22 * k, 12 * k);
        let tags = format!(
            "\\an{an}\\move({x},{y},{},{},0,{})\\1c{ghost_colour}\\3c{ghost_colour}\\alpha{alpha}{size}{blur}{}",
            x + dx,
            y + dy,
            280 + 80 * k,
            fad(line, 0, 300 + 100 * i64::from(k)),
        );
        ev(line, fx, names::ECHO, &tags, &words).on_layer(base)
    };
    let main = |tags: String| ev(line, fx, names::ECHO, &tags, &words).on_layer(base + 1);
    match tier {
        Tier::Low => vec![ev(line, fx, names::ECHO, &format!("{}\\bord5", super::place_tags(fx.placement)), &words)],
        Tier::Tv => vec![
            ghost(1, "&H70&", ""),
            main(format!("\\an{an}\\pos({x},{y})\\bord4{}", fad(line, 60, 0))),
        ],
        Tier::Desktop if title => vec![
            ghost(2, "&HA0&", "\\blur4"),
            ghost(1, "&H70&", "\\blur2"),
            main(format!(
                "\\an{an}\\pos({x},{y}){size}\\bord6\\fscx70\\fscy70\\t(0,250,\\fscx100\\fscy100){}",
                fad(line, 120, 200)
            )),
        ],
        Tier::Desktop => vec![
            ghost(2, "&HA0&", "\\blur3"),
            ghost(1, "&H70&", "\\blur1.5"),
            main(format!(
                "\\an{an}\\pos({x},{y})\\bord4\\fscx112\\fscy112\\t(0,100,\\fscx100\\fscy100){}",
                fad(line, 40, 0)
            )),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use crate::plan::{Intent, Tier};

    #[test]
    fn low_is_one_static_event() {
        let evs = run(&line("Getsuga!"), &fx(Intent::AttackCall, 2), Tier::Low);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].style, "Echo");
        assert_tags(&evs[0], &["\\bord5"], &["\\move", "\\t(", "\\fad"]);
    }

    #[test]
    fn tv_has_one_ghost_and_no_blur() {
        let evs = run(&line("Getsuga!"), &fx(Intent::AttackCall, 2), Tier::Tv);
        assert_eq!(evs.len(), 2);
        assert_tags(&evs[0], &["\\move(", "\\fad(0,"], &["\\blur", "\\t("]);
        assert_tags(&evs[1], &["\\fad(60,0)"], &["\\blur", "\\t(", "\\move"]);
        assert!(evs[0].layer < evs[1].layer);
    }

    #[test]
    fn desktop_has_two_blurred_ghosts_trailing_further_each() {
        let evs = run(&line("Getsuga!"), &fx(Intent::AttackCall, 2), Tier::Desktop);
        assert_eq!(evs.len(), 3);
        assert_tags(&evs[0], &["\\blur3", "\\move("], &[]);
        assert_tags(&evs[1], &["\\blur1.5", "\\move("], &[]);
        assert_tags(&evs[2], &["\\t(0,100,"], &["\\blur", "\\move"]);
    }

    #[test]
    fn full_intensity_on_desktop_is_a_title_card() {
        let evs = run(&line("Getsuga!"), &fx(Intent::AttackCall, 3), Tier::Desktop);
        assert_eq!(evs.len(), 3);
        for e in &evs {
            assert_tags(e, &["\\an5", "\\fs130"], &[]);
        }
        // Not on TV: it keeps the ordinary form.
        let tv = run(&line("Getsuga!"), &fx(Intent::AttackCall, 3), Tier::Tv);
        assert_tags(&tv[1], &[], &["\\fs130", "\\an5"]);
    }
}
