//! The effect library: one module per effect, each turning a source line
//! into the events that draw it at a tier (spec 028 FR-003).
//!
//! Every effect keeps to the tier budgets by construction. Low is static
//! styling (face, size, colour, border) in one event. TV spends at most one
//! `\t` or `\move` per event, no blur, and at most two events per line.
//! Desktop spends at most four `\t` and one `\move` per event and at most
//! four events per line. The designs are Pharos's own.

mod aside;
mod burst;
mod caption;
mod echo;
mod giggle;
mod hush;
mod impact;
mod inner_voice;
mod karaoke;
mod memory;
mod tremor;
mod wire;

use crate::event::Event;
use crate::plan::{Intent, Karaoke, LineEffect, Placement, Tier};
use crate::source::SourceLine;
use crate::theme::Theme;

/// Tags for a line's placement; empty for the style's own spot.
pub(crate) fn place_tags(p: Placement) -> String {
    match p {
        Placement::Default => String::new(),
        Placement::Top => "\\an8".to_owned(),
        Placement::Free { x, y } => format!("\\an5\\pos({x},{y})"),
    }
}

/// Alignment and anchor point for effects that need explicit `\move`
/// coordinates: bottom centre, top centre, or a free point.
pub(crate) fn anchor(theme: &Theme, style: &str, p: Placement) -> (u8, i32, i32) {
    let (al, mv) = theme
        .style(style)
        .map_or((2, 54), |s| (s.alignment, s.margin_v));
    let cx = theme.play_res_x / 2;
    match p {
        Placement::Free { x, y } => (5, x, y),
        Placement::Top => (8, cx, if al >= 7 { mv } else { 54 }),
        Placement::Default if al >= 7 => (8, cx, mv),
        Placement::Default => (2, cx, theme.play_res_y - mv),
    }
}

/// A deterministic 64-bit hash of a line index, for jitter and tilt.
pub(crate) fn seed(index: usize) -> u64 {
    let mut z = (index as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `\fad(a,b)` with each side limited to a third of the line.
pub(crate) fn fad(line: &SourceLine, a: i64, b: i64) -> String {
    let cap = ((line.end_ms - line.start_ms) / 3).max(0);
    format!("\\fad({},{})", a.min(cap), b.min(cap))
}

/// An event with `tags` in one override block ahead of `text`.
pub(crate) fn ev(line: &SourceLine, fx: &LineEffect, style: &str, tags: &str, text: &str) -> Event {
    let text = if tags.is_empty() {
        text.to_owned()
    } else {
        format!("{{{tags}}}{text}")
    };
    Event::on(line, fx.line, style, text)
}

/// The events for `fx` on `line` at `tier`, or `None` when the intent has no
/// effect of its own (Plain, Sarcasm) and the line is drawn plain.
pub(crate) fn events(
    line: &SourceLine,
    fx: &LineEffect,
    theme: &Theme,
    tier: Tier,
) -> Option<Vec<Event>> {
    if fx.karaoke != Karaoke::None && karaoke::syllables(line, fx).is_some() {
        return Some(karaoke::events(line, fx, theme, tier));
    }
    match fx.intent {
        Intent::Narration => Some(caption::events(line, fx, theme, tier)),
        Intent::OffScreen => Some(aside::events(line, fx, theme, tier)),
        Intent::Sfx => Some(burst::events(line, fx, theme, tier)),
        Intent::Laugh => Some(giggle::events(line, fx, theme, tier)),
        Intent::AttackCall => Some(echo::events(line, fx, theme, tier)),
        Intent::Phone => Some(wire::events(line, fx, theme, tier)),
        Intent::Flashback => Some(memory::events(line, fx, theme, tier)),
        Intent::Thought => Some(inner_voice::events(line, fx, theme, tier)),
        Intent::Shout if fx.intensity >= 3 => Some(tremor::events(line, fx, theme, tier)),
        Intent::Shout => Some(impact::events(line, fx, theme, tier)),
        Intent::Whisper => Some(hush::events(line, fx, theme, tier)),
        Intent::Plain | Intent::Sarcasm => None,
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::plan::Karaoke;
    use crate::source::SourceFormat;

    pub fn line(text: &str) -> SourceLine {
        SourceLine {
            start_ms: 10_000,
            end_ms: 13_000,
            text: text.to_owned(),
            raw: text.to_owned(),
            format: SourceFormat::Srt,
            style: "Default".into(),
            layer: 0,
            italic: false,
            typeset: false,
            karaoke: None,
            style_def: None,
        }
    }

    pub fn fx(intent: Intent, intensity: u8) -> LineEffect {
        LineEffect {
            line: 7,
            intent,
            intensity,
            placement: Placement::Default,
            karaoke: Karaoke::None,
            reason: String::new(),
            static_only: false,
            syllables: None,
        }
    }

    pub fn run(l: &SourceLine, f: &LineEffect, tier: Tier) -> Vec<Event> {
        events(l, f, &Theme::pharos(), tier).expect("intent has an effect")
    }

    /// `(has, lacks)` over the first event's text.
    pub fn assert_tags(e: &Event, has: &[&str], lacks: &[&str]) {
        for h in has {
            assert!(e.text.contains(h), "{h} missing from {}", e.text);
        }
        for l in lacks {
            assert!(!e.text.contains(l), "{l} present in {}", e.text);
        }
    }
}
