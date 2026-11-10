//! Telling a viewer that a file is damaged and where (B940): a badge on its
//! tile, a line on its title page, a short note when it starts playing and
//! the damaged spans on the seek bar. Where playback breaks on damage, where
//! a skip should carry on from, and the spans this device skipped (B1160).

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
        // B1162: nothing is broken to mark, but a seek reads the file from
        // the start, so a jump near the end can take a minute.
        ("unindexed", _) => {
            "This file has no seek index. Skipping ahead can take up to a minute.".to_owned()
        }
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

/// A skip past the end of a damaged span lands this far after it, clear of
/// the packets either side of the scan's edge (B1160).
pub const SKIP_PAST_MS: u64 = 3_000;
/// With no span known, a skip jumps this far past where playback broke.
pub const SKIP_STEP_MS: u64 = 10_000;
/// A failure this soon before a scanned span is that span: the decoders read
/// a few seconds ahead of the picture.
const SKIP_AHEAD_MS: u64 = 15_000;
/// A failure again soon after a skip widens the next one, up to this.
const SKIP_MAX_MS: u64 = 300_000;
/// A skip that would land this close to the end offers nothing to watch.
const SKIP_END_MS: u64 = 5_000;
/// Spans remembered per file; the oldest go first.
const REMEMBERED: usize = 16;

/// Where a skip's target came from, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipSource {
    /// The integrity scan's span.
    Scan,
    /// A span this device skipped before, failing at the same place.
    Remembered,
    /// The last skip landed on more damage: a wider jump.
    Again,
    /// Nothing known: a fixed step.
    Step,
}

impl SkipSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Remembered => "remembered",
            Self::Again => "again",
            Self::Step => "step",
        }
    }
}

/// Where to carry on after playback broke at `at_ms`, or `None` when the
/// skip would land at the end. `scan` is the integrity scan's spans for the
/// file playing (empty when they describe another version); `skipped` is
/// what this device skipped in it before.
pub fn skip_target(
    at_ms: u64,
    runtime_ms: u64,
    scan: &[DamageSpan],
    skipped: &[DamageSpan],
) -> Option<(u64, SkipSource)> {
    let again = skipped
        .iter()
        .find(|s| at_ms + NEAR_MS >= s.start_ms && at_ms <= s.end_ms + SKIP_STEP_MS);
    let (to_ms, source) = if let Some(s) = again {
        if at_ms < s.end_ms {
            // Broke where it broke before: land where that skip landed.
            (s.end_ms, SkipSource::Remembered)
        } else {
            // Landed on more damage: jump twice as far as last time.
            let width = (s.end_ms - s.start_ms).max(SKIP_STEP_MS);
            (at_ms + (2 * width).min(SKIP_MAX_MS), SkipSource::Again)
        }
    } else if let Some(s) = scan
        .iter()
        .find(|s| s.start_ms <= at_ms + SKIP_AHEAD_MS && at_ms <= s.end_ms + NEAR_MS)
    {
        (s.end_ms.max(at_ms) + SKIP_PAST_MS, SkipSource::Scan)
    } else {
        (at_ms + SKIP_STEP_MS, SkipSource::Step)
    };
    (runtime_ms == 0 || to_ms + SKIP_END_MS < runtime_ms).then_some((to_ms, source))
}

/// `skipped` with `from..to` added, overlapping spans merged, the oldest
/// dropped past the cap.
pub fn remember_skip(skipped: &[DamageSpan], from_ms: u64, to_ms: u64) -> Vec<DamageSpan> {
    let mut merged = DamageSpan {
        start_ms: from_ms.min(to_ms),
        end_ms: to_ms.max(from_ms),
    };
    let mut kept: Vec<DamageSpan> = Vec::with_capacity(skipped.len() + 1);
    for s in skipped {
        if s.start_ms <= merged.end_ms && merged.start_ms <= s.end_ms {
            merged.start_ms = merged.start_ms.min(s.start_ms);
            merged.end_ms = merged.end_ms.max(s.end_ms);
        } else {
            kept.push(*s);
        }
    }
    kept.push(merged);
    let over = kept.len().saturating_sub(REMEMBERED);
    kept.drain(..over);
    kept
}

/// Where this device keeps the spans it skipped in one file.
pub fn skipped_key(item: impl std::fmt::Display, version: &str) -> String {
    if version.is_empty() {
        format!("pharos.skipped.{item}")
    } else {
        format!("pharos.skipped.{item}.{version}")
    }
}

/// `1000-5000,9000-12000`; unreadable parts are dropped.
pub fn parse_skipped(stored: &str) -> Vec<DamageSpan> {
    stored
        .split(',')
        .filter_map(|part| {
            let (a, b) = part.trim().split_once('-')?;
            let (start_ms, end_ms) = (a.parse().ok()?, b.parse().ok()?);
            (start_ms <= end_ms).then_some(DamageSpan { start_ms, end_ms })
        })
        .take(REMEMBERED)
        .collect()
}

pub fn format_skipped(spans: &[DamageSpan]) -> String {
    spans
        .iter()
        .map(|s| format!("{}-{}", s.start_ms, s.end_ms))
        .collect::<Vec<_>>()
        .join(",")
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
    fn a_skip_clears_a_scanned_span_even_when_the_decoder_broke_early() {
        let scan = [span(600_000, 612_000)];
        // Broke 10 s before the span starts: the decoder reads ahead.
        assert_eq!(
            skip_target(590_000, 3_600_000, &scan, &[]),
            Some((615_000, SkipSource::Scan))
        );
        // Broke inside it.
        assert_eq!(
            skip_target(605_000, 3_600_000, &scan, &[]),
            Some((615_000, SkipSource::Scan))
        );
        // Far from any span: a fixed step.
        assert_eq!(
            skip_target(100_000, 3_600_000, &scan, &[]),
            Some((110_000, SkipSource::Step))
        );
    }

    #[test]
    fn a_skip_widens_when_it_lands_on_more_damage_and_reuses_a_good_landing() {
        let skipped = [span(100_000, 110_000)];
        // Broke again 2 s after landing: twice the last jump.
        assert_eq!(
            skip_target(112_000, 3_600_000, &[], &skipped),
            Some((132_000, SkipSource::Again))
        );
        // A later play breaks where it broke before: land where it landed.
        assert_eq!(
            skip_target(100_500, 3_600_000, &[], &skipped),
            Some((110_000, SkipSource::Remembered))
        );
        // The widening is capped.
        let wide = [span(0, 1_000_000)];
        assert_eq!(
            skip_target(1_001_000, 3_600_000, &[], &wide),
            Some((1_301_000, SkipSource::Again))
        );
    }

    #[test]
    fn no_skip_is_offered_into_the_last_seconds() {
        assert_eq!(skip_target(1_990_000, 2_000_000, &[], &[]), None);
        assert_eq!(
            skip_target(1_980_000, 2_000_000, &[], &[]),
            Some((1_990_000, SkipSource::Step))
        );
        // An unknown runtime does not stop a skip.
        assert!(skip_target(1_990_000, 0, &[], &[]).is_some());
    }

    #[test]
    fn skipped_spans_merge_cap_and_round_trip() {
        let one = remember_skip(&[], 100_000, 110_000);
        assert_eq!(one, [span(100_000, 110_000)]);
        let two = remember_skip(&one, 108_000, 130_000);
        assert_eq!(two, [span(100_000, 130_000)]);
        let three = remember_skip(&two, 500_000, 510_000);
        assert_eq!(three.len(), 2);
        assert_eq!(parse_skipped(&format_skipped(&three)), three);
        assert_eq!(parse_skipped("1-2,junk,9-3,4-5"), [span(1, 2), span(4, 5)]);
        let mut many = Vec::new();
        for i in 0..20 {
            many = remember_skip(&many, i * 100_000, i * 100_000 + 1_000);
        }
        assert_eq!(many.len(), REMEMBERED);
        assert_eq!(many[0].start_ms, 400_000, "the oldest go first");
        assert_eq!(skipped_key(7, ""), "pharos.skipped.7");
        assert_eq!(skipped_key(7, "v2"), "pharos.skipped.7.v2");
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
        let unindexed = Damage {
            verdict: "unindexed".into(),
            ..Damage::default()
        };
        assert_eq!(
            warning_line(&unindexed),
            "This file has no seek index. Skipping ahead can take up to a minute."
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
