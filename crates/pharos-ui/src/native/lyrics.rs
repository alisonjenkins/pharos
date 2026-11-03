//! A song's lyrics on the now-playing screen (013 US7, B360/V300). Synced
//! lyrics follow the playhead: the line being sung is lit and kept in the
//! middle of the panel (once the listener's own scrolling has rested), and a line seeks to itself when chosen. Plain
//! lyrics are text to scroll through.

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

#[component]
pub fn LyricsPanel(lyrics: Lyrics, position_ms: u64, on_seek: EventHandler<u64>) -> Element {
    let current = current_line(&lyrics, position_ms);
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
    fn plain_lyrics_have_no_current_line() {
        let mut l = synced(&[0, 0]);
        l.synced = false;
        assert_eq!(current_line(&l, 10_000), None);
    }

    fn render(lyrics: Lyrics, position_ms: u64) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                rsx! {
                    LyricsPanel { lyrics: lyrics.clone(), position_ms, on_seek: |_| {} }
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
    fn an_empty_synced_line_shows_as_an_instrumental_gap() {
        let mut l = synced(&[0, 2_000]);
        l.lines[1].text = String::new();
        let html = render(l, 0);
        assert!(html.contains("pharos-lyrics-gap"), "{html}");
    }
}
