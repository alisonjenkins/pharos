//! The tier budget checker (spec 028 FR-004, FR-006, plan.md "Tier budgets").
//!
//! It reads compiled ASS back with its own override-tag scanner, so the
//! property test checks the compiler against an independent reader.

use crate::plan::Tier;
use crate::source::{self, AssEvent};
use crate::tags::{self, Tag};

/// The tags the output may use (FR-004).
const ALLOWED: &[&str] = &[
    "t", "move", "fad", "fade", "k", "kf", "K", "ko", "frx", "fry", "frz", "fscx", "fscy", "fsp",
    "blur", "be", "bord", "shad", "clip", "iclip", "p", "org", "pos", "an", "c", "1c", "2c", "3c",
    "4c", "alpha", "1a", "2a", "3a", "4a", "fn", "fs", "b", "i", "r",
];

/// Most events that may start in any 10 s window (6 per second on average).
const MAX_STARTS_PER_10S: usize = 60;
const WINDOW_MS: i64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// The ASS could not be read back.
    Unreadable,
    /// A tag outside the FR-004 allow-list.
    ForbiddenTag,
    /// More `\t` / `\move` than the tier allows in one event.
    TooManyTransforms,
    /// `\fad` / `\fade` where the tier has none.
    FadeNotAllowed,
    /// `\blur` / `\be` where the tier has none.
    BlurNotAllowed,
    /// Karaoke tags the tier does not allow.
    KaraokeNotAllowed,
    /// More animated events on screen at one instant than the tier allows.
    TooManyAnimated,
    /// More than 6 event starts per second averaged over a 10 s window.
    StartRate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breach {
    /// Zero-based index among the file's `Dialogue:` lines; 0 for file-wide rules.
    pub event: usize,
    pub rule: Rule,
    pub detail: String,
}

fn breach(event: usize, rule: Rule, detail: impl Into<String>) -> Breach {
    Breach {
        event,
        rule,
        detail: detail.into(),
    }
}

/// Most animated events that may be on screen at one instant.
pub fn max_animated(tier: Tier) -> usize {
    match tier {
        Tier::Low => 0,
        Tier::Tv => 3,
        Tier::Desktop => 8,
    }
}

/// Most events a single source line may become.
pub fn max_events_per_line(tier: Tier) -> usize {
    match tier {
        Tier::Low => 1,
        Tier::Tv => 2,
        Tier::Desktop => 4,
    }
}

/// True when the tags make the event count toward the on-screen budget.
pub(crate) fn is_animated(tags: &[Tag]) -> bool {
    tags.iter().any(|t| tags::is_animation(&t.name))
}

/// The Name column prefix the compiler gives events it copied through from
/// the source unchanged (FR-005).
pub const PASSTHROUGH_PREFIX: &str = "sign:";

/// Checks compiled `ass` against `tier`. All breaches are returned.
///
/// Events marked as passthrough (Name starts with [`PASSTHROUGH_PREFIX`]) are
/// the source's own typesetting and are exempt from every rule, including the
/// on-screen and start-rate counts. The start-rate rule also cannot be met by
/// a source that itself starts more than 60 events in 10 s: the compiler never
/// drops lines, so such a file breaches `StartRate` however it is styled.
pub fn check(ass: &str, tier: Tier) -> Result<(), Vec<Breach>> {
    let all = match source::ass_events(ass) {
        Ok(e) => e,
        Err(e) => return Err(vec![breach(0, Rule::Unreadable, e.to_string())]),
    };
    // Keep each event's index in the file for breach reports.
    let events: Vec<(usize, AssEvent<'_>)> = all
        .into_iter()
        .enumerate()
        .filter(|(_, e)| !e.name.starts_with(PASSTHROUGH_PREFIX))
        .collect();
    let mut out = Vec::new();
    let mut animated: Vec<(i64, i64, usize)> = Vec::new();
    for (i, ev) in &events {
        let (i, ev) = (*i, ev);
        let tags = tags::scan_text(ev.text);
        check_event(i, &tags, tier, &mut out);
        if is_animated(&tags) {
            animated.push((ev.start_ms, ev.end_ms, i));
        }
    }
    check_concurrency(&animated, tier, &mut out);
    check_start_rate(&events, &mut out);
    if out.is_empty() {
        Ok(())
    } else {
        Err(out)
    }
}

fn check_event(i: usize, tags: &[Tag], tier: Tier, out: &mut Vec<Breach>) {
    let count = |names: &[&str], top_only: bool| {
        tags.iter()
            .filter(|t| names.contains(&t.name.as_str()) && !(top_only && t.nested))
            .count()
    };
    for t in tags {
        if !ALLOWED.contains(&t.name.as_str()) {
            out.push(breach(i, Rule::ForbiddenTag, format!("\\{}", t.name)));
        }
    }
    let t = count(&["t"], true);
    let mv = count(&["move"], true);
    let fade = count(&["fad", "fade"], true);
    let blur = count(&["blur", "be"], false);
    let k_all = count(&["k", "kf", "K", "ko"], true);
    let k_other = count(&["k", "K", "ko"], true);
    match tier {
        Tier::Low => {
            if t + mv > 0 {
                out.push(breach(
                    i,
                    Rule::TooManyTransforms,
                    "\\t or \\move at the low tier",
                ));
            }
            if fade > 0 {
                out.push(breach(i, Rule::FadeNotAllowed, "\\fad at the low tier"));
            }
            if blur > 0 {
                out.push(breach(
                    i,
                    Rule::BlurNotAllowed,
                    "\\blur or \\be at the low tier",
                ));
            }
            if k_all > 0 {
                out.push(breach(
                    i,
                    Rule::KaraokeNotAllowed,
                    "karaoke at the low tier",
                ));
            }
        }
        Tier::Tv => {
            if t + mv > 1 {
                out.push(breach(
                    i,
                    Rule::TooManyTransforms,
                    format!("{t} \\t and {mv} \\move; the TV tier allows one in all"),
                ));
            }
            if blur > 0 {
                out.push(breach(
                    i,
                    Rule::BlurNotAllowed,
                    "\\blur or \\be at the TV tier",
                ));
            }
            if k_other > 0 {
                out.push(breach(
                    i,
                    Rule::KaraokeNotAllowed,
                    "only \\kf at the TV tier",
                ));
            }
        }
        Tier::Desktop => {
            if t > 4 || mv > 1 {
                out.push(breach(
                    i,
                    Rule::TooManyTransforms,
                    format!("{t} \\t and {mv} \\move; the desktop tier allows 4 and 1"),
                ));
            }
        }
    }
}

/// An event is on screen over `[start, end)`; the peak overlap is always
/// reached at some event's start.
fn check_concurrency(animated: &[(i64, i64, usize)], tier: Tier, out: &mut Vec<Breach>) {
    let cap = max_animated(tier);
    if cap == 0 {
        // Every animated event already breaches its own tag rule.
        return;
    }
    for &(s, _, i) in animated {
        let n = animated
            .iter()
            .filter(|&&(a, b, _)| a <= s && s < b)
            .count();
        if n > cap {
            out.push(breach(
                i,
                Rule::TooManyAnimated,
                format!("{n} animated events on screen at {s} ms; the cap is {cap}"),
            ));
        }
    }
}

fn check_start_rate(events: &[(usize, AssEvent<'_>)], out: &mut Vec<Breach>) {
    let mut starts: Vec<i64> = events.iter().map(|(_, e)| e.start_ms).collect();
    starts.sort_unstable();
    for (j, &s) in starts.iter().enumerate() {
        let n = starts[j..]
            .iter()
            .take_while(|&&t| t < s + WINDOW_MS)
            .count();
        if n > MAX_STARTS_PER_10S {
            out.push(breach(
                0,
                Rule::StartRate,
                format!("{n} events start within 10 s of {s} ms; the cap is {MAX_STARTS_PER_10S}"),
            ));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `(start_ms, end_ms, text)` events as a minimal ASS file.
    fn ass(events: &[(i64, i64, &str)]) -> String {
        let mut s = String::from("[Script Info]\nScriptType: v4.00+\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
        for (a, b, t) in events {
            let cs = |ms: i64| format!("0:00:{:02}.{:02}", ms / 1000, (ms % 1000) / 10);
            s.push_str(&format!(
                "Dialogue: 0,{},{},Default,,0,0,0,,{t}\n",
                cs(*a),
                cs(*b)
            ));
        }
        s
    }

    fn rules(events: &[(i64, i64, &str)], tier: Tier) -> Vec<Rule> {
        match check(&ass(events), tier) {
            Ok(()) => vec![],
            Err(b) => b.into_iter().map(|b| b.rule).collect(),
        }
    }

    const ONE: i64 = 1000;

    #[test]
    fn plain_text_passes_everywhere() {
        for tier in Tier::ALL {
            assert!(check(&ass(&[(0, ONE, r"{\an8\i1\bord3}hi")]), tier).is_ok());
        }
    }

    #[test]
    fn tag_outside_the_allow_list_is_a_breach_at_every_tier() {
        for tier in Tier::ALL {
            assert_eq!(
                rules(&[(0, ONE, r"{\fax0.3}hi")], tier),
                [Rule::ForbiddenTag]
            );
            assert_eq!(rules(&[(0, ONE, r"{\a6}hi")], tier), [Rule::ForbiddenTag]);
            assert_eq!(rules(&[(0, ONE, r"{\q2}hi")], tier), [Rule::ForbiddenTag]);
        }
    }

    #[test]
    fn nested_tags_are_checked_too() {
        assert_eq!(
            rules(&[(0, ONE, r"{\t(0,100,\fax1)}hi")], Tier::Desktop),
            [Rule::ForbiddenTag]
        );
        assert_eq!(
            rules(&[(0, ONE, r"{\t(0,100,\blur3)}hi")], Tier::Tv),
            [Rule::BlurNotAllowed]
        );
    }

    #[test]
    fn low_allows_no_animation_at_all() {
        let low = |t: &str| rules(&[(0, ONE, t)], Tier::Low);
        assert_eq!(low(r"{\t(0,100,\fscx90)}x"), [Rule::TooManyTransforms]);
        assert_eq!(low(r"{\move(0,0,5,5)}x"), [Rule::TooManyTransforms]);
        assert_eq!(low(r"{\fad(100,100)}x"), [Rule::FadeNotAllowed]);
        assert_eq!(low(r"{\blur2}x"), [Rule::BlurNotAllowed]);
        assert_eq!(low(r"{\be1}x"), [Rule::BlurNotAllowed]);
        assert_eq!(low(r"{\kf20}x"), [Rule::KaraokeNotAllowed]);
    }

    #[test]
    fn tv_allows_one_transform_and_fade_but_no_blur() {
        let tv = |t: &str| rules(&[(0, ONE, t)], Tier::Tv);
        assert!(tv(r"{\t(0,100,\fscx90)\fad(50,50)}x").is_empty());
        assert!(tv(r"{\move(0,0,5,5)\fad(50,50)}x").is_empty());
        assert_eq!(
            tv(r"{\t(0,10,\frz1)\t(10,20,\frz2)}x"),
            [Rule::TooManyTransforms]
        );
        assert_eq!(
            tv(r"{\t(0,10,\frz1)\move(0,0,1,1)}x"),
            [Rule::TooManyTransforms]
        );
        assert_eq!(tv(r"{\blur1}x"), [Rule::BlurNotAllowed]);
        assert_eq!(tv(r"{\be1}x"), [Rule::BlurNotAllowed]);
    }

    #[test]
    fn tv_karaoke_is_kf_only() {
        let tv = |t: &str| rules(&[(0, ONE, t)], Tier::Tv);
        assert!(tv(r"{\kf20}a{\kf30}b").is_empty());
        assert_eq!(tv(r"{\k20}a"), [Rule::KaraokeNotAllowed]);
        assert_eq!(tv(r"{\K20}a"), [Rule::KaraokeNotAllowed]);
        assert_eq!(tv(r"{\ko20}a"), [Rule::KaraokeNotAllowed]);
    }

    #[test]
    fn desktop_allows_four_t_and_one_move() {
        let d = |t: &str| rules(&[(0, ONE, t)], Tier::Desktop);
        assert!(d(r"{\t(0,1,\frz1)\t(1,2,\frz2)\t(2,3,\frz3)\t(3,4,\frz4)\blur2\k20}x").is_empty());
        assert_eq!(
            d(r"{\t(0,1,\frz1)\t(1,2,\frz2)\t(2,3,\frz3)\t(3,4,\frz4)\t(4,5,\frz5)}x"),
            [Rule::TooManyTransforms]
        );
        assert_eq!(
            d(r"{\move(0,0,1,1)\move(0,0,2,2)}x"),
            [Rule::TooManyTransforms]
        );
    }

    #[test]
    fn animated_events_on_screen_are_capped() {
        let four: Vec<_> = (0..4).map(|_| (0, 5 * ONE, r"{\fad(10,10)}x")).collect();
        assert_eq!(rules(&four, Tier::Tv), [Rule::TooManyAnimated; 1].repeat(4));
        assert!(rules(&four[..3], Tier::Tv).is_empty());
        let nine: Vec<_> = (0..9).map(|_| (0, 5 * ONE, r"{\fad(10,10)}x")).collect();
        assert!(!rules(&nine, Tier::Desktop).is_empty());
        assert!(rules(&nine[..8], Tier::Desktop).is_empty());
        // Sequential events never overlap.
        let seq: Vec<_> = (0..9)
            .map(|i| (i * ONE, i * ONE + 900, r"{\fad(10,10)}x"))
            .collect();
        assert!(rules(&seq, Tier::Tv).is_empty());
    }

    #[test]
    fn static_events_do_not_count_as_animated() {
        let many: Vec<_> = (0..20).map(|_| (0, 5 * ONE, r"{\bord3}x")).collect();
        assert!(rules(&many, Tier::Tv).is_empty());
    }

    #[test]
    fn start_rate_is_capped_over_ten_seconds() {
        // 61 starts inside 10 s.
        let ok: Vec<_> = (0..60).map(|i| (i * 150, i * 150 + 100, "x")).collect();
        let bad: Vec<_> = (0..61).map(|i| (i * 150, i * 150 + 100, "x")).collect();
        assert!(rules(&ok, Tier::Desktop).is_empty());
        assert_eq!(rules(&bad, Tier::Desktop), [Rule::StartRate]);
    }

    #[test]
    fn passthrough_events_are_exempt_from_every_rule() {
        let mut f = ass(&[(0, 5 * ONE, r"{\pos(3,2)\t(0,1,\fax1)\blur3\k5}X")]);
        // Not exempt until the compiler marks it.
        assert!(check(&f, Tier::Low).is_err());
        f = f.replace("Default,,0,0,0,,", "Default,sign:0,0,0,0,,");
        assert!(check(&f, Tier::Low).is_ok());
        // Nor do marked events count toward the on-screen or start-rate caps.
        let many: Vec<_> = (0..80)
            .map(|i| (i * 10, 5 * ONE, r"{\fad(1,1)}x"))
            .collect();
        let f = ass(&many).replace("Default,,0,0,0,,", "Default,sign:1,0,0,0,,");
        assert!(check(&f, Tier::Tv).is_ok());
    }

    #[test]
    fn unreadable_ass_is_reported() {
        let r = check("[Events]\nDialogue: 0,0:00:01.00\n", Tier::Low).unwrap_err();
        assert_eq!(r[0].rule, Rule::Unreadable);
    }
}
