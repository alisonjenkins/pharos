//! Telling a viewer that a file is damaged and where (B940): a badge on its
//! tile, a line on its title page, a short note when it starts playing and
//! the damaged spans on the seek bar.

use dioxus::prelude::*;
use pharos_client::api::{Damage, DamageSpan};

use super::playback::clock;

/// Spans named in a sentence before the rest are counted.
const SPANS_NAMED: usize = 3;

/// A pointer this close to a span a packet wide is on it: such a span is
/// drawn wider than it is, so the preview agrees with what the bar shows.
const NEAR_MS: u64 = 1_000;

/// `10:00–19:00, 33:20 and 2 more`; a span one packet wide is one time.
pub fn spans_label(spans: &[DamageSpan]) -> String {
    let mut parts: Vec<String> = spans
        .iter()
        .take(SPANS_NAMED)
        .map(|s| {
            if s.end_ms / 1_000 > s.start_ms / 1_000 {
                format!("{}–{}", clock(s.start_ms), clock(s.end_ms))
            } else {
                clock(s.start_ms)
            }
        })
        .collect();
    let more = spans.len().saturating_sub(SPANS_NAMED);
    if more > 0 {
        parts.push(format!("{more} more"));
    }
    match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// What the viewer is told: where the file is damaged and what that does.
pub fn warning_line(damage: &Damage) -> String {
    let at = spans_label(&damage.ranges);
    match (damage.verdict.as_str(), at.is_empty()) {
        ("short", true) => "Part of this file is missing. It may stop early.".to_owned(),
        ("short", false) => format!("Part of this file is missing at {at}. It may stop early."),
        (_, true) => "This file is damaged. Playback may skip or stop.".to_owned(),
        (_, false) => format!("This file is damaged at {at}. Playback may skip or stop there."),
    }
}

/// Whether `ms` falls in one of the spans.
pub fn in_span(spans: &[DamageSpan], ms: u64) -> bool {
    spans
        .iter()
        .any(|s| ms + NEAR_MS >= s.start_ms && ms <= s.end_ms + NEAR_MS)
}

/// The title page's line. The reader's own words are too technical to lead
/// with, so they are the line's tooltip.
#[component]
pub fn DamageLine(damage: Damage) -> Element {
    rsx! {
        p { class: "pharos-damage-line", title: "{damage.reason}",
            span { class: "pharos-damage-mark", aria_hidden: "true", "⚠" }
            " {warning_line(&damage)}"
        }
    }
}

/// The player's note when a damaged file starts. Over the video and outside
/// the chrome; it fades by itself after a few seconds and takes no pointer,
/// so the picture stays the viewer's.
#[component]
pub fn PlayerDamageNote(damage: Damage) -> Element {
    rsx! {
        div { class: "pharos-player-damage", role: "status", aria_live: "polite",
            span { class: "pharos-damage-mark", aria_hidden: "true", "⚠" }
            " {warning_line(&damage)}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_ms: u64, end_ms: u64) -> DamageSpan {
        DamageSpan { start_ms, end_ms }
    }

    #[test]
    fn spans_read_as_times_and_a_long_list_is_counted() {
        assert_eq!(spans_label(&[]), "");
        assert_eq!(spans_label(&[span(600_000, 1_140_000)]), "10:00–19:00");
        assert_eq!(
            spans_label(&[span(600_000, 1_140_000), span(2_000_000, 2_000_040)]),
            "10:00–19:00 and 33:20"
        );
        let many: Vec<_> = (0..5)
            .map(|i| span(i * 60_000, i * 60_000 + 5_000))
            .collect();
        assert_eq!(
            spans_label(&many),
            "0:00–0:05, 1:00–1:05, 2:00–2:05 and 2 more"
        );
    }

    #[test]
    fn the_warning_says_where_and_what_it_does() {
        let damaged = Damage {
            verdict: "damaged".into(),
            ranges: vec![span(600_000, 1_140_000)],
            ..Damage::default()
        };
        assert_eq!(
            warning_line(&damaged),
            "This file is damaged at 10:00–19:00. Playback may skip or stop there."
        );
        let short = Damage {
            verdict: "short".into(),
            ranges: vec![span(3_000_000, 5_400_000)],
            ..Damage::default()
        };
        assert_eq!(
            warning_line(&short),
            "Part of this file is missing at 50:00–1:30:00. It may stop early."
        );
        let nowhere = Damage {
            verdict: "unreadable".into(),
            ..Damage::default()
        };
        assert_eq!(
            warning_line(&nowhere),
            "This file is damaged. Playback may skip or stop."
        );
    }

    #[test]
    fn a_packet_wide_span_catches_a_pointer_near_it() {
        let spans = [span(2_000_000, 2_000_040), span(600_000, 1_140_000)];
        assert!(in_span(&spans, 2_000_500));
        assert!(in_span(&spans, 1_999_200));
        assert!(in_span(&spans, 900_000));
        assert!(!in_span(&spans, 300_000));
        assert!(!in_span(&spans, 2_100_000));
    }
}
