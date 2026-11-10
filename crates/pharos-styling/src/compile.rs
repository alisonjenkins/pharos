//! Plan, source lines, theme, tier and level to ASS v4+ text (spec 028 FR-002).
//!
//! Pure and deterministic: the same inputs give the same bytes.

use std::collections::{BTreeMap, BTreeSet};

use crate::budget::{max_animated, max_events_per_line, PASSTHROUGH_PREFIX};
use crate::effects;
use crate::event::Event;
use crate::plan::{EffectPlan, EffectsLevel, Intent, Karaoke, LineEffect, Placement, Tier};
use crate::source::SourceLine;
use crate::text;
use crate::theme::{names, StyleDef, Theme, STYLE_FORMAT};
use crate::version::COMPILER_VERSION;

const WINDOW_MS: i64 = 10_000;
const MAX_STARTS_PER_WINDOW: usize = 60;
/// Two lines this close or closer read as one exchange, so the change from
/// one to the next is eased rather than cut (B1206).
const EASE_GAP_MS: i64 = 250;
/// The eased line's fade in and its predecessor's fade out.
const EASE_IN_MS: i64 = 120;
const EASE_OUT_MS: i64 = 100;
/// How far, in script units, an eased line rises into place on Desktop, and
/// over how long.
const EASE_RISE: i32 = 14;
const EASE_RISE_MS: i64 = 160;

/// Compiles `lines` as directed by `plan`.
///
/// - `Off` draws every line plain; `Subtle` caps intensity at 1 and keeps
///   Ember (a Full-only karaoke fill) as Sweep.
/// - Lines with their own typesetting are copied through verbatim (FR-005).
/// - Unless the level is `Off`, a plain line that follows another within
///   [`EASE_GAP_MS`], with nothing else on screen, fades in (and on Desktop
///   rises a little into place); one followed that closely fades out (B1206).
/// - When a line's effect would break a budget that depends on its
///   neighbours (animated events on screen, event starts per 10 s), the line
///   falls back to the next cheaper tier's form of the effect.
/// - Exception: the compiler never drops lines, so a source that itself
///   starts more than 60 events in 10 s breaches the start-rate rule however
///   it is styled. Passthrough events are named `sign:<index>` and are exempt
///   from `budget::check`.
pub fn compile(
    plan: &EffectPlan,
    lines: &[SourceLine],
    theme: &Theme,
    tier: Tier,
    level: EffectsLevel,
) -> String {
    let by_line: BTreeMap<usize, &LineEffect> = plan.lines.iter().map(|l| (l.line, l)).collect();
    let mut placer = Placer::new(tier);
    let mut events: Vec<(usize, Event)> = Vec::new();
    let mut styles: BTreeSet<String> = BTreeSet::new();
    styles.insert(names::DEFAULT.to_owned());
    // Source style definitions kept verbatim, by their `Format:` line and name.
    let mut verbatim: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut aliases: BTreeSet<String> = BTreeSet::new();

    for line in lines {
        placer.reserve(line);
    }
    // Typeset events go through unchanged and are counted first, so the
    // restyled lines are fitted around them.
    for (i, line) in lines.iter().enumerate() {
        if !line.typeset {
            continue;
        }
        let style = if line.style.is_empty() {
            names::DEFAULT
        } else {
            &line.style
        };
        let mut ev = Event::on(line, i, style, line.raw.clone());
        ev.name = format!("{PASSTHROUGH_PREFIX}{i}");
        if ev.is_visible() {
            if theme.style(style).is_some() {
                // A theme style of that name wins over the source's.
                styles.insert(style.to_owned());
            } else if let Some(def) = &line.style_def {
                verbatim
                    .entry(def.format.clone())
                    .or_default()
                    .insert(style.to_owned(), def.line.clone());
            } else {
                aliases.insert(style.to_owned());
            }
            placer.accept(std::slice::from_ref(&ev));
            events.push((i, ev));
        }
    }
    let eases = if level == EffectsLevel::Off || tier == Tier::Low {
        vec![Ease::default(); lines.len()]
    } else {
        eases(lines)
    };
    for (i, line) in lines.iter().enumerate() {
        if line.typeset || line.text.is_empty() {
            continue;
        }
        let fx = effective(by_line.get(&i).copied(), line, i, level);
        let ease = eases.get(i).copied().unwrap_or_default();
        let Some(evs) = fit(line, &fx, ease, theme, tier, &placer) else {
            continue;
        };
        placer.accept(&evs);
        let sized = (theme.tag_scale - 1.0).abs() > f32::EPSILON;
        for mut ev in evs {
            if sized {
                ev.text = crate::tags::scale_sizes(&ev.text, theme.tag_scale);
            }
            styles.insert(ev.style.clone());
            events.push((i, ev));
        }
    }
    events.sort_by(|(ia, a), (ib, b)| {
        (a.start_ms, a.layer, *ia, &a.text).cmp(&(b.start_ms, b.layer, *ib, &b.text))
    });

    let mut out = String::new();
    out.push_str("[Script Info]\n");
    out.push_str(&format!("; pharos-styling compiler {COMPILER_VERSION}\n"));
    out.push_str(
        "Title: Pharos styled\nScriptType: v4.00+\nWrapStyle: 0\nScaledBorderAndShadow: yes\n",
    );
    out.push_str(&format!(
        "PlayResX: {}\nPlayResY: {}\n\n",
        theme.play_res_x, theme.play_res_y
    ));
    out.push_str("[V4+ Styles]\n");
    out.push_str(STYLE_FORMAT);
    out.push('\n');
    for s in theme.styles.iter().filter(|s| styles.contains(&s.name)) {
        out.push_str(&s.render());
        out.push('\n');
    }
    // A typeset event whose style the source does not define resolves to the
    // dialogue style rather than to nothing.
    for name in &aliases {
        if let Some(base) = theme.style(names::DEFAULT) {
            let alias = StyleDef {
                name: name.clone(),
                ..base.clone()
            };
            out.push_str(&alias.render());
            out.push('\n');
        }
    }
    // The source's own styles for its signs, under the source's own columns.
    for (format, defs) in &verbatim {
        out.push_str(if format.is_empty() {
            STYLE_FORMAT
        } else {
            format
        });
        out.push('\n');
        for line in defs.values() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("\n[Events]\n");
    out.push_str(
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    for (_, ev) in &events {
        out.push_str(&ev.render());
        out.push('\n');
    }
    out
}

/// The plan entry after applying the effects level and the line's own data.
fn effective(
    fx: Option<&LineEffect>,
    line: &SourceLine,
    index: usize,
    level: EffectsLevel,
) -> LineEffect {
    let plain = LineEffect {
        line: index,
        intent: Intent::Plain,
        intensity: 0,
        placement: Placement::Default,
        karaoke: Karaoke::None,
        reason: String::new(),
        static_only: false,
    };
    let Some(fx) = fx else { return plain };
    let mut fx = fx.clone();
    fx.line = index;
    match level {
        EffectsLevel::Off => return plain,
        EffectsLevel::Subtle => {
            fx.intensity = fx.intensity.min(1);
            if fx.karaoke == Karaoke::Ember {
                fx.karaoke = Karaoke::Sweep;
            }
        }
        EffectsLevel::Full => fx.intensity = fx.intensity.min(3),
    }
    if line.karaoke.is_none() {
        fx.karaoke = Karaoke::None;
    }
    if fx.intensity == 0 {
        fx.intent = Intent::Plain;
    }
    fx
}

/// Whether a line eases in from the line before it and out into the line
/// after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ease {
    into: bool,
    out_of: bool,
}

/// Each line's [`Ease`], by index. Only a line alone on screen eases: an
/// eased plain line is positioned, which turns off the renderer's collision
/// handling, so a line overlapping another keeps its normal placement.
fn eases(lines: &[SourceLine]) -> Vec<Ease> {
    let mut out = vec![Ease::default(); lines.len()];
    let mut order: Vec<usize> = (0..lines.len())
        .filter(|&i| !lines[i].typeset && !lines[i].text.is_empty())
        .collect();
    order.sort_by_key(|&i| (lines[i].start_ms, lines[i].end_ms, i));
    // Sorted by start, a line shares the screen with an earlier line when it
    // starts before the latest end so far, and with a later one when the next
    // line starts before it ends.
    let mut alone = Vec::with_capacity(order.len());
    let mut latest_end = i64::MIN;
    for (k, &i) in order.iter().enumerate() {
        let l = &lines[i];
        let after = order
            .get(k + 1)
            .map_or(true, |&j| lines[j].start_ms >= l.end_ms);
        alone.push(l.start_ms >= latest_end && after);
        latest_end = latest_end.max(l.end_ms);
    }
    for k in 1..order.len() {
        let (a, b) = (order[k - 1], order[k]);
        let gap = lines[b].start_ms - lines[a].end_ms;
        if alone[k - 1] && alone[k] && (0..=EASE_GAP_MS).contains(&gap) {
            out[a].out_of = true;
            out[b].into = true;
        }
    }
    out
}

/// The events for the line at the richest tier up to `tier` that fits.
fn fit(
    line: &SourceLine,
    fx: &LineEffect,
    ease: Ease,
    theme: &Theme,
    tier: Tier,
    placer: &Placer,
) -> Option<Vec<Event>> {
    let mut t = if fx.static_only { Tier::Low } else { tier };
    loop {
        let evs: Vec<Event> = effects::events(line, fx, theme, t)
            .unwrap_or_else(|| vec![plain(line, fx, ease, theme, t)])
            .into_iter()
            .filter(Event::is_visible)
            .collect();
        if evs.is_empty() {
            return None;
        }
        if t == Tier::Low || (evs.len() <= max_events_per_line(tier) && placer.fits(&evs)) {
            return Some(evs);
        }
        t = t.down();
    }
}

/// The dialogue style with the source's own italics kept, eased in or out
/// when `ease` says so and the line has its default placement.
fn plain(line: &SourceLine, fx: &LineEffect, ease: Ease, theme: &Theme, tier: Tier) -> Event {
    let body = text::body(line);
    let mut tags = effects::place_tags(fx.placement);
    if fx.placement == Placement::Default && tier != Tier::Low && (ease.into || ease.out_of) {
        let fade_in = if ease.into { EASE_IN_MS } else { 0 };
        let fade_out = if ease.out_of { EASE_OUT_MS } else { 0 };
        let (an, x, y) = effects::anchor(theme, names::DEFAULT, Placement::Default);
        if tier == Tier::Desktop && ease.into {
            tags.push_str(&format!(
                "\\an{an}\\move({x},{},{x},{y},0,{EASE_RISE_MS})",
                y + EASE_RISE
            ));
        } else {
            tags.push_str(&format!("\\an{an}\\pos({x},{y})"));
        }
        tags.push_str(&effects::fad(line, fade_in, fade_out));
    }
    let text = format!("{}{}", brace(&tags), body);
    Event::on(line, fx.line, names::DEFAULT, text)
}

fn brace(tags: &str) -> String {
    if tags.is_empty() {
        String::new()
    } else {
        format!("{{{tags}}}")
    }
}

/// Tracks what has been placed, to keep the cross-line budgets.
struct Placer {
    tier: Tier,
    /// Every event start that is, or will be, in the file.
    starts: Vec<i64>,
    animated: Vec<(i64, i64)>,
}

impl Placer {
    fn new(tier: Tier) -> Placer {
        Placer {
            tier,
            starts: Vec::new(),
            animated: Vec::new(),
        }
    }

    fn fits(&self, evs: &[Event]) -> bool {
        self.fits_animated(evs) && self.fits_rate(evs)
    }

    /// Counts the line's own start up front, so a line's extra events are
    /// fitted around the starts of lines that come after it.
    fn reserve(&mut self, line: &SourceLine) {
        let probe = Event::on(line, 0, names::DEFAULT, String::new());
        if probe.is_visible() && (line.typeset || !line.text.is_empty()) {
            self.insert_start(probe.start_ms);
        }
    }

    fn insert_start(&mut self, ms: i64) {
        let at = self.starts.partition_point(|&s| s <= ms);
        self.starts.insert(at, ms);
    }

    /// Records a line's events. Its first event's start was reserved.
    fn accept(&mut self, evs: &[Event]) {
        for (k, e) in evs.iter().enumerate() {
            if k > 0 {
                self.insert_start(e.start_ms);
            }
            if e.is_animated() {
                self.animated.push((e.start_ms, e.end_ms));
            }
        }
    }

    fn fits_animated(&self, evs: &[Event]) -> bool {
        let cap = max_animated(self.tier);
        let cand: Vec<(i64, i64)> = evs
            .iter()
            .filter(|e| e.is_animated())
            .map(|e| (e.start_ms, e.end_ms))
            .collect();
        if cand.is_empty() {
            return true;
        }
        if cap == 0 {
            return false;
        }
        // Peak overlap is always reached at some event's start.
        cand.iter().all(|&(s, _)| {
            let n = self
                .animated
                .iter()
                .chain(&cand)
                .filter(|&&(a, b)| a <= s && s < b)
                .count();
            n <= cap
        })
    }

    fn fits_rate(&self, evs: &[Event]) -> bool {
        let Some(first) = evs.first() else {
            return true;
        };
        let mut merged: Vec<i64> = self.starts.clone();
        for e in evs.iter().skip(1) {
            let at = merged.partition_point(|&s| s <= e.start_ms);
            merged.insert(at, e.start_ms);
        }
        let lo = first.start_ms - WINDOW_MS;
        let hi = evs
            .iter()
            .map(|e| e.start_ms)
            .max()
            .unwrap_or(first.start_ms);
        merged
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s > lo && s <= hi)
            .all(|(j, &s)| {
                merged[j..]
                    .iter()
                    .take_while(|&&t| t < s + WINDOW_MS)
                    .count()
                    <= MAX_STARTS_PER_WINDOW
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::source;
    use crate::version::DIRECTOR_VERSION;

    const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nHello <i>there</i>\n\n2\n00:00:04,000 --> 00:00:05,500\n<i>Slanted</i>\nsecond line\n\n3\n00:00:06,000 --> 00:00:07,000\nThe end\n";

    fn lines() -> Vec<SourceLine> {
        source::read(SRT).unwrap().lines
    }

    fn plain_plan(n: usize) -> EffectPlan {
        EffectPlan {
            director_version: DIRECTOR_VERSION,
            lines: (0..n)
                .map(|line| LineEffect {
                    line,
                    intent: Intent::Plain,
                    intensity: 0,
                    placement: Placement::Default,
                    karaoke: Karaoke::None,
                    reason: String::new(),
                    static_only: false,
                })
                .collect(),
        }
    }

    fn run(plan: &EffectPlan, lines: &[SourceLine], tier: Tier, level: EffectsLevel) -> String {
        compile(plan, lines, &Theme::pharos(), tier, level)
    }

    #[test]
    fn a_static_only_line_is_drawn_in_its_low_form_at_any_tier() {
        let lines = lines();
        let mut plan = plain_plan(lines.len());
        plan.lines[0].intent = Intent::Shout;
        plan.lines[0].intensity = 2;
        let animated = run(&plan, &lines, Tier::Desktop, EffectsLevel::Full);
        plan.lines[0].static_only = true;
        let still = run(&plan, &lines, Tier::Desktop, EffectsLevel::Full);
        let low = run(&plan, &lines, Tier::Low, EffectsLevel::Full);
        assert_ne!(animated, still, "the flag must change the Desktop output");
        assert_eq!(dialogue(&still)[0], dialogue(&low)[0]);
        assert!(!dialogue(&still)[0].contains("\\t("));
    }

    fn dialogue(ass: &str) -> Vec<&str> {
        ass.lines().filter(|l| l.starts_with("Dialogue:")).collect()
    }

    #[test]
    fn an_all_plain_plan_uses_the_plain_style_only() {
        let ass = run(&plain_plan(3), &lines(), Tier::Desktop, EffectsLevel::Full);
        let styles: Vec<_> = ass.lines().filter(|l| l.starts_with("Style:")).collect();
        assert_eq!(styles.len(), 1);
        assert!(styles[0].starts_with("Style: Default,Liberation Sans,"));
        assert_eq!(dialogue(&ass).len(), 3);
        assert!(dialogue(&ass).iter().all(|d| d.contains(",Default,")));
    }

    #[test]
    fn plain_text_keeps_italics_and_hard_breaks() {
        let ass = run(&plain_plan(3), &lines(), Tier::Low, EffectsLevel::Full);
        let d = dialogue(&ass);
        assert!(d[0].ends_with(",,Hello {\\i1}there{\\i0}"), "{}", d[0]);
        assert!(
            d[1].ends_with(",,{\\i1}Slanted{\\i0}\\Nsecond line"),
            "{}",
            d[1]
        );
    }

    /// Three lines a beat apart, then one long after, then two at once.
    const CLOSE: &str = "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
        2\n00:00:02,100 --> 00:00:03,000\nTwo\n\n\
        3\n00:00:03,000 --> 00:00:04,000\nThree\n\n\
        4\n00:00:09,000 --> 00:00:10,000\nFour\n\n\
        5\n00:00:10,100 --> 00:00:12,000\nFive\n\n\
        6\n00:00:11,000 --> 00:00:12,000\nSix\n";

    /// B1206: lines a beat apart ease from one to the next; a line with
    /// another on screen, or far from its neighbours, does not.
    #[test]
    fn close_lines_ease_into_each_other() {
        let lines = source::read(CLOSE).unwrap().lines;
        let e = eases(&lines);
        let flags: Vec<(bool, bool)> = e.iter().map(|e| (e.into, e.out_of)).collect();
        assert_eq!(
            flags,
            [
                (false, true),
                (true, true),
                (true, false),
                (false, false),
                (false, false),
                (false, false)
            ]
        );
        let plan = plain_plan(lines.len());
        let desk = run(&plan, &lines, Tier::Desktop, EffectsLevel::Subtle);
        let d = dialogue(&desk);
        assert!(
            d[0].contains("\\an2\\pos(960,1026)\\fad(0,100)}One"),
            "{}",
            d[0]
        );
        assert!(
            d[1].contains("\\an2\\move(960,1040,960,1026,0,160)\\fad(120,100)}Two"),
            "{}",
            d[1]
        );
        assert!(d[3].ends_with(",,Four"), "{}", d[3]);
        assert!(d[4].ends_with(",,Five") && d[5].ends_with(",,Six"));
        let tv = run(&plan, &lines, Tier::Tv, EffectsLevel::Full);
        assert!(dialogue(&tv)[1].contains("\\pos(960,1026)\\fad(120,100)}Two"));
        assert!(!tv.contains("\\move"));
        for (tier, level) in [
            (Tier::Low, EffectsLevel::Full),
            (Tier::Desktop, EffectsLevel::Off),
        ] {
            let ass = run(&plan, &lines, tier, level);
            assert!(
                !ass.contains("\\fad") && !ass.contains("\\pos"),
                "{tier:?} {level:?}"
            );
        }
        for tier in Tier::ALL {
            let ass = run(&plan, &lines, tier, EffectsLevel::Full);
            assert_eq!(crate::budget::check(&ass, tier), Ok(()), "{tier:?}");
        }
    }

    #[test]
    fn same_inputs_same_bytes() {
        let plan = plain_plan(3);
        let a = run(&plan, &lines(), Tier::Tv, EffectsLevel::Subtle);
        let b = run(&plan, &lines(), Tier::Tv, EffectsLevel::Subtle);
        assert_eq!(a, b);
    }

    #[test]
    fn output_parses_with_no_bad_lines() {
        for tier in Tier::ALL {
            let ass = run(&plain_plan(3), &lines(), tier, EffectsLevel::Full);
            let script = pharos_subs::Script::parse(&ass);
            assert_eq!(script.stats.bad_lines, 0, "{ass}");
            assert_eq!(script.events.len(), 3);
        }
    }

    #[test]
    fn events_stay_inside_their_source_line() {
        let ass = run(&plain_plan(3), &lines(), Tier::Low, EffectsLevel::Full);
        assert!(dialogue(&ass)[1].starts_with("Dialogue: 0,0:00:04.00,0:00:05.50,"));
    }

    #[test]
    fn a_missing_plan_entry_is_plain() {
        let ass = run(&plain_plan(0), &lines(), Tier::Low, EffectsLevel::Full);
        assert_eq!(dialogue(&ass).len(), 3);
    }

    #[test]
    fn placement_is_kept_on_plain_lines() {
        let mut plan = plain_plan(3);
        plan.lines[0].placement = Placement::Top;
        plan.lines[1].placement = Placement::Free { x: 100, y: 200 };
        plan.lines[0].intensity = 1;
        let ass = run(&plan, &lines(), Tier::Low, EffectsLevel::Full);
        let d = dialogue(&ass);
        assert!(d[0].contains(",,{\\an8}Hello"), "{}", d[0]);
        assert!(
            d[1].contains(",,{\\an5\\pos(100,200)}{\\i1}Slanted"),
            "{}",
            d[1]
        );
    }

    const SIGN_ASS: &str = "[Script Info]\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, Alignment\nStyle: Sign,Impact,90,8\nStyle: Default,Arial,20,2\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 3,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\pos(3,2)\\t(0,1,\\fax1)\\blur3}Baratie, east\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,Talk\nDialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,{\\an8}Top\n";

    #[test]
    fn typeset_events_pass_through_with_their_own_style_line() {
        let src = source::read(SIGN_ASS).unwrap().lines;
        assert!(src[0].typeset && !src[1].typeset && src[2].typeset);
        let out = run(&plain_plan(3), &src, Tier::Low, EffectsLevel::Off);
        let d = dialogue(&out);
        assert_eq!(
            d[0],
            "Dialogue: 3,0:00:01.00,0:00:02.00,Sign,sign:0,0,0,0,,{\\pos(3,2)\\t(0,1,\\fax1)\\blur3}Baratie, east"
        );
        // The source's Format and Style lines, verbatim, once.
        assert_eq!(
            out.matches("Format: Name, Fontname, Fontsize, Alignment\n")
                .count(),
            1
        );
        assert_eq!(out.matches("Style: Sign,Impact,90,8\n").count(), 1);
        // A signed event on a theme style name keeps the theme's style only.
        assert_eq!(out.matches("Style: Default,").count(), 1);
        assert!(out.contains("Style: Default,Liberation Sans,"));
        assert!(!out.contains("Arial"));
        assert_eq!(pharos_subs::Script::parse(&out).stats.bad_lines, 0);
        // The sign breaches the Low tier on its own; it is exempt.
        crate::budget::check(&out, Tier::Low).unwrap();
    }

    #[test]
    fn a_typeset_event_without_a_source_style_uses_the_dialogue_style() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Gone,,0,0,0,,{\\pos(1,1)}x\n";
        let src = source::read(ass).unwrap().lines;
        let out = run(&plain_plan(1), &src, Tier::Low, EffectsLevel::Full);
        assert!(out.contains("\nStyle: Gone,Liberation Sans,"));
    }

    #[test]
    fn a_source_denser_than_six_starts_a_second_breaches_only_the_rate_rule() {
        // 70 plain lines starting 100 ms apart: the compiler never drops
        // lines, so the start-rate rule cannot hold; nothing else breaches.
        let mut srt = String::new();
        for i in 0..70 {
            let s = 1000 + i * 100;
            srt.push_str(&format!(
                "{}\n00:00:{:02},{:03} --> 00:00:{:02},{:03}\nx\n\n",
                i + 1,
                s / 1000,
                s % 1000,
                (s + 90) / 1000,
                (s + 90) % 1000
            ));
        }
        let src = source::read(&srt).unwrap().lines;
        for tier in Tier::ALL {
            let ass = run(&plain_plan(70), &src, tier, EffectsLevel::Full);
            let breaches = crate::budget::check(&ass, tier).unwrap_err();
            assert!(
                breaches
                    .iter()
                    .all(|b| b.rule == crate::budget::Rule::StartRate),
                "{breaches:?}"
            );
        }
    }

    #[test]
    fn lines_with_no_visible_text_are_dropped() {
        let src = source::read("1\n00:00:01,000 --> 00:00:02,000\n<i></i>\n")
            .unwrap()
            .lines;
        let ass = run(&plain_plan(1), &src, Tier::Low, EffectsLevel::Full);
        assert!(dialogue(&ass).is_empty());
    }

    #[test]
    fn braces_in_dialogue_cannot_open_override_blocks() {
        let src = source::read("1\n00:00:01,000 --> 00:00:02,000\na {\\fax9} b }{ c\\N\n")
            .unwrap()
            .lines;
        let ass = run(&plain_plan(1), &src, Tier::Desktop, EffectsLevel::Full);
        crate::budget::check(&ass, Tier::Desktop).unwrap();
    }

    /// Every intent at every intensity, placement, tier and level, on one
    /// busy source: the output parses, keeps the budgets and stays in range.
    #[test]
    fn every_effect_keeps_every_budget() {
        let mut srt = String::new();
        let mut script_lines = Vec::new();
        for i in 0..Intent::ALL.len() * 4 {
            let s = 2000 * i as i64;
            script_lines.push((s, s + 1800));
            srt.push_str(&format!(
                "{}\n{} --> {}\n(Line {i}) it is [boom] <i>now</i>\n\n",
                i + 1,
                ts(s),
                ts(s + 1800)
            ));
        }
        let lines = source::read(&srt).unwrap().lines;
        for tier in Tier::ALL {
            for level in EffectsLevel::ALL {
                for placement in [
                    Placement::Default,
                    Placement::Top,
                    Placement::Free { x: 300, y: 400 },
                ] {
                    let plan = EffectPlan {
                        director_version: DIRECTOR_VERSION,
                        lines: lines
                            .iter()
                            .enumerate()
                            .map(|(line, _)| LineEffect {
                                line,
                                intent: Intent::ALL[line / 4],
                                intensity: (line % 4) as u8,
                                placement,
                                karaoke: Karaoke::None,
                                reason: String::new(),
                                static_only: false,
                            })
                            .collect(),
                    };
                    let ass = run(&plan, &lines, tier, level);
                    let ctx = format!("{tier:?} {level:?} {placement:?}\n{ass}");
                    assert_eq!(pharos_subs::Script::parse(&ass).stats.bad_lines, 0, "{ctx}");
                    if let Err(b) = crate::budget::check(&ass, tier) {
                        panic!("{b:?}\n{ctx}");
                    }
                    let mut per_line: BTreeMap<usize, usize> = BTreeMap::new();
                    for d in dialogue(&ass) {
                        let f: Vec<&str> = d.splitn(10, ',').collect();
                        let idx: usize = f[4].parse().unwrap();
                        *per_line.entry(idx).or_default() += 1;
                        let (a, b) = script_lines[idx];
                        let t = |s: &str| source::parse_time(s).unwrap();
                        assert!(t(f[1]) >= a && t(f[2]) <= b, "{d}");
                    }
                    assert!(
                        per_line
                            .values()
                            .all(|&n| n <= crate::budget::max_events_per_line(tier)),
                        "{ctx}"
                    );
                }
            }
        }
    }

    fn ts(ms: i64) -> String {
        format!(
            "00:{:02}:{:02},{:03}",
            ms / 60_000,
            ms / 1000 % 60,
            ms % 1000
        )
    }
}
