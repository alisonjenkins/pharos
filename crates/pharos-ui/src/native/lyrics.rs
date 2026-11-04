//! A song's lyrics on the now-playing screen (013 US7, B360/V300). Synced
//! lyrics follow the playhead: the line being sung is lit and kept in the
//! middle of the panel (once the listener's own scrolling has rested), and a line seeks to itself when chosen. Plain
//! lyrics are text to scroll through.
//!
//! Karaoke mode (B483/V427), when the listener turns it on: the line being
//! sung is drawn large and filled in from left to right as it is sung. LRC
//! times lines, not words, so the fill runs evenly from a line's start to
//! the next line's.

use std::cell::Cell;
use std::rc::Rc;

use dioxus::prelude::*;
use pharos_client::api::Lyrics;

use super::platform;

pub const PANEL_ID: &str = "pharos-lyrics";

/// The player reports its position about four times a second, so a line lit
/// on time would show up to a quarter second late; lighting it this early
/// lands it on the word more often than not.
const LEAD_MS: u64 = 200;

/// How long the panel leaves the listener's own scrolling alone before it
/// follows the song again.
const HOLD_MS: f64 = 4_000.0;

/// How long the last line fills for, having no next line to end at.
const LAST_LINE_MS: u64 = 5_000;

/// The index of the line being sung at `position_ms`: the last line that has
/// started. `None` before the first line.
pub fn current_line(lyrics: &Lyrics, position_ms: u64) -> Option<usize> {
    if !lyrics.synced {
        return None;
    }
    let at = position_ms.saturating_add(LEAD_MS);
    lyrics
        .lines
        .partition_point(|l| l.start_ms <= at)
        .checked_sub(1)
}

/// How far through line `i` the singer is at `position_ms`, in percent: 0
/// at its start, 100 at the next line's start (or [`LAST_LINE_MS`] later
/// for the last line).
pub fn line_progress(lyrics: &Lyrics, i: usize, position_ms: u64) -> f64 {
    let Some(line) = lyrics.lines.get(i) else {
        return 0.0;
    };
    let end = lyrics
        .lines
        .get(i + 1)
        .map_or(line.start_ms + LAST_LINE_MS, |l| l.start_ms);
    let span = end.saturating_sub(line.start_ms);
    if span == 0 {
        return 100.0;
    }
    // Lines are seconds long, so these fit an f64 exactly.
    #[allow(clippy::cast_precision_loss)]
    let done = position_ms.saturating_sub(line.start_ms) as f64 / span as f64;
    (done * 100.0).clamp(0.0, 100.0)
}

#[component]
pub fn LyricsPanel(
    lyrics: Lyrics,
    position_ms: u64,
    on_seek: EventHandler<u64>,
    /// Draw the sung line large and fill it in as it is sung.
    #[props(default)]
    karaoke: bool,
) -> Element {
    let current = current_line(&lyrics, position_ms);
    let karaoke = karaoke && lyrics.synced;
    let mut held_until = use_signal(|| 0.0_f64);
    // The line lit most recently, for a centring put off by a hold.
    let lit = use_hook(|| Rc::new(Cell::new(None::<usize>)));
    use_effect(use_reactive!(|current| {
        lit.set(current);
        if current.is_none() {
            return;
        }
        let wait = *held_until.peek() - platform::now_ms();
        if wait <= 0.0 {
            center(current);
            return;
        }
        let lit = Rc::clone(&lit);
        spawn(async move {
            // A hold is at most HOLD_MS, so this fits a u32.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            platform::sleep_ms(wait.ceil() as u32).await;
            if platform::now_ms() >= *held_until.peek() {
                center(lit.get());
            }
        });
    }));
    let mut hold = move || held_until.set(platform::now_ms() + HOLD_MS);
    rsx! {
        div {
            id: PANEL_ID,
            class: "pharos-lyrics",
            role: "region",
            aria_label: "Lyrics",
            tabindex: 0,
            "data-synced": if lyrics.synced { "true" } else { "false" },
            "data-karaoke": if karaoke { "true" } else { "false" },
            // The screen's wheel moves the volume; here it scrolls the words.
            onwheel: move |evt: WheelEvent| {
                evt.stop_propagation();
                hold();
            },
            ontouchstart: move |_| hold(),
            onkeydown: move |evt: KeyboardEvent| {
                if matches!(evt.key(), Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown) {
                    evt.stop_propagation();
                    hold();
                }
            },
            if lyrics.synced {
                for (i, line) in lyrics.lines.iter().enumerate() {
                    button {
                        key: "{i}",
                        class: "pharos-lyrics-line",
                        r#type: "button",
                        "data-state": line_state(i, current),
                        aria_current: if Some(i) == current { "true" } else { "false" },
                        onclick: {
                            let start = line.start_ms;
                            move |_| on_seek.call(start)
                        },
                        if line.text.trim().is_empty() {
                            span { class: "pharos-lyrics-gap", aria_label: "Instrumental", "♪" }
                        } else if karaoke && Some(i) == current {
                            span {
                                class: "pharos-lyrics-words",
                                // The same lead that picked the line, so it is full
                                // just as the next line takes over.
                                style: "--pharos-wipe: {line_progress(&lyrics, i, position_ms.saturating_add(LEAD_MS)):.1}%",
                                "{line.text}"
                            }
                        } else {
                            "{line.text}"
                        }
                    }
                }
            } else {
                for (i, line) in lyrics.lines.iter().enumerate() {
                    if line.text.trim().is_empty() {
                        div { key: "{i}", class: "pharos-lyrics-break", aria_hidden: "true" }
                    } else {
                        p { key: "{i}", class: "pharos-lyrics-line", "{line.text}" }
                    }
                }
            }
        }
    }
}

fn center(line: Option<usize>) {
    if let Some(at) = line {
        platform::center_child(PANEL_ID, u32::try_from(at).unwrap_or(u32::MAX));
    }
}

fn line_state(i: usize, current: Option<usize>) -> &'static str {
    match current {
        Some(c) if i == c => "current",
        Some(c) if i < c => "past",
        _ => "next",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_client::api::LyricLine;

    fn synced(starts: &[u64]) -> Lyrics {
        Lyrics {
            synced: true,
            lines: starts
                .iter()
                .enumerate()
                .map(|(i, &start_ms)| LyricLine {
                    start_ms,
                    text: format!("line {i}"),
                })
                .collect(),
        }
    }

    #[test]
    fn the_current_line_is_the_last_one_started_a_little_early() {
        let l = synced(&[1_000, 3_000, 5_000]);
        assert_eq!(current_line(&l, 0), None);
        assert_eq!(current_line(&l, 799), None);
        assert_eq!(current_line(&l, 800), Some(0));
        assert_eq!(current_line(&l, 2_900), Some(1));
        assert_eq!(current_line(&l, 60_000), Some(2));
    }

    #[test]
    fn a_line_fills_from_its_start_to_the_next_lines() {
        let l = synced(&[1_000, 3_000, 5_000]);
        assert!((line_progress(&l, 0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((line_progress(&l, 0, 2_000) - 50.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 9_000) - 100.0).abs() < f64::EPSILON);
        // The last line has no next one, so it fills over LAST_LINE_MS.
        assert!((line_progress(&l, 2, 5_000 + LAST_LINE_MS / 2) - 50.0).abs() < 1e-9);
        // Two lines at one time: the first is already done.
        assert!((line_progress(&synced(&[0, 0]), 0, 0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn plain_lyrics_have_no_current_line() {
        let mut l = synced(&[0, 0]);
        l.synced = false;
        assert_eq!(current_line(&l, 10_000), None);
    }

    fn render(lyrics: Lyrics, position_ms: u64) -> String {
        render_as(lyrics, position_ms, false)
    }

    fn render_as(lyrics: Lyrics, position_ms: u64, karaoke: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    LyricsPanel { lyrics: lyrics.clone(), position_ms, on_seek: |_| {}, karaoke }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn synced_lines_mark_past_current_and_next() {
        let html = render(synced(&[1_000, 3_000, 5_000]), 3_500);
        let states: Vec<_> = html
            .match_indices("data-state=\"")
            .map(|(i, m)| {
                let rest = &html[i + m.len()..];
                &rest[..rest.find('"').unwrap()]
            })
            .collect();
        assert_eq!(states, ["past", "current", "next"]);
        assert_eq!(html.matches(r#"aria-current="true""#).count(), 1);
        assert!(html.contains(r#"data-synced="true""#), "{html}");
    }

    #[test]
    fn plain_lyrics_are_paragraphs_with_breaks_between_verses() {
        let lines = ["One", "", "Two"]
            .iter()
            .map(|t| LyricLine {
                start_ms: 0,
                text: (*t).to_owned(),
            })
            .collect();
        let html = render(
            Lyrics {
                synced: false,
                lines,
            },
            0,
        );
        assert_eq!(html.matches("<p class=\"pharos-lyrics-line\"").count(), 2);
        assert_eq!(html.matches("pharos-lyrics-break").count(), 1);
        assert!(!html.contains("<button"), "{html}");
    }

    #[test]
    fn karaoke_fills_only_the_line_being_sung() {
        let l = synced(&[1_000, 3_000, 5_000]);
        // 2 s in plus the 200 ms lead is 60% of the first line.
        let html = render_as(l.clone(), 2_000, true);
        assert!(html.contains(r#"data-karaoke="true""#), "{html}");
        assert_eq!(html.matches("pharos-lyrics-words").count(), 1, "{html}");
        assert!(html.contains("--pharos-wipe: 60.0%"), "{html}");
        // The last frame before the next line takes over shows it full.
        let edge = render_as(l.clone(), 3_000 - LEAD_MS - 1, true);
        assert!(edge.contains("--pharos-wipe: 100.0%"), "{edge}");
        // Off, or on plain lyrics, nothing fills.
        let off = render_as(l.clone(), 2_000, false);
        assert!(!off.contains("pharos-lyrics-words"), "{off}");
        assert!(off.contains(r#"data-karaoke="false""#), "{off}");
        let mut plain = l;
        plain.synced = false;
        let plain = render_as(plain, 2_000, true);
        assert!(plain.contains(r#"data-karaoke="false""#), "{plain}");
    }

    #[test]
    fn an_empty_synced_line_shows_as_an_instrumental_gap() {
        let mut l = synced(&[0, 2_000]);
        l.lines[1].text = String::new();
        let html = render(l, 0);
        assert!(html.contains("pharos-lyrics-gap"), "{html}");
    }
}
