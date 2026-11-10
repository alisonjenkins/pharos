//! A song's lyrics on the now-playing screen (013 US7, B360/V300). Synced
//! lyrics follow the playhead: the line being sung is lit and kept in the
//! middle of the panel (once the listener's own scrolling has rested), and a line seeks to itself when chosen. Plain
//! lyrics are text to scroll through.
//!
//! Karaoke mode (B483/V427), when the listener turns it on: the line being
//! sung is drawn large and filled in from left to right as it is sung. When a
//! line carries enhanced-LRC word times (B496), the fill follows them;
//! otherwise it runs across each word by its syllables and ends early when a
//! pause follows.

use std::cell::Cell;
use std::rc::Rc;

use dioxus::prelude::*;
use pharos_client::api::{LyricLine, Lyrics};

use super::platform;

pub const PANEL_ID: &str = "pharos-lyrics";

/// The player reports its position about four times a second, so a line lit
/// on time would show up to a quarter second late; lighting it this early
/// lands it on the word more often than not.
const LEAD_MS: u64 = 200;

/// The player reports its position about every 250 ms and the CSS eases the
/// fill over the same time, so the fill aims one report ahead and arrives as
/// the next report lands.
const REPORT_MS: u64 = 250;

/// How long the panel leaves the listener's own scrolling alone before it
/// follows the song again.
const HOLD_MS: f64 = 4_000.0;

/// How long the last line fills for, having no next line to end at.
const LAST_LINE_MS: u64 = 5_000;

/// The longest a sung syllable is assumed to take; a line whose next line is
/// further off than its syllables need is taken to end early, leaving the
/// rest as a pause.
const SYLLABLE_MS: u64 = 700;

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

/// About how many syllables `word` is sung in: its groups of vowels, less a
/// silent final "e". Words with no Latin letters count a syllable a character.
fn syllables(word: &str) -> u32 {
    let mut groups = 0u32;
    let mut in_vowels = false;
    let mut prev_letter = None::<char>;
    let mut last_letter = None::<char>;
    for c in word.chars() {
        if matches!(c, '\'' | '’') {
            continue;
        }
        if c.is_alphabetic() && (c as u32) < 0x250 {
            let lower = c.to_lowercase().next().unwrap_or(c);
            let is_vowel = "aeiouyàáâãäåæèéêëìíîïòóôõöøùúûüýÿœ".contains(lower);
            if is_vowel && !in_vowels {
                groups += 1;
            }
            in_vowels = is_vowel;
            prev_letter = last_letter;
            last_letter = Some(lower);
        } else {
            in_vowels = false;
        }
    }
    if last_letter.is_none() {
        return u32::try_from(word.chars().count())
            .unwrap_or(u32::MAX)
            .max(1);
    }
    if groups > 1 && last_letter == Some('e') && prev_letter != Some('l') {
        groups -= 1;
    }
    groups.max(1)
}

/// How far through line `i` the singer is at `position_ms`, in percent: 0 at
/// its start, following its word timestamps when it has them or weighted
/// across its words by syllable count, and reaching 100 at the next line's
/// start (or [`LAST_LINE_MS`] later for the last line) or after
/// [`SYLLABLE_MS`] per syllable when a pause follows.
pub fn line_progress(lyrics: &Lyrics, i: usize, position_ms: u64) -> f64 {
    let Some(line) = lyrics.lines.get(i) else {
        return 0.0;
    };
    let end = lyrics
        .lines
        .get(i + 1)
        .map_or(line.start_ms.saturating_add(LAST_LINE_MS), |l| l.start_ms);
    if !line.words.is_empty() {
        return word_progress(line, end, position_ms);
    }
    let span = end.saturating_sub(line.start_ms);
    if span == 0 {
        return 100.0;
    }
    let mut words = Vec::new();
    let mut in_word = None::<(usize, usize, usize)>;
    let mut char_count = 0usize;
    for (idx, (byte_idx, ch)) in line.text.char_indices().enumerate() {
        char_count = idx + 1;
        if ch.is_whitespace() {
            if let Some((start_byte, offset, len)) = in_word.take() {
                let s = syllables(&line.text[start_byte..byte_idx]);
                words.push((offset, len, s));
            }
        } else if let Some((_, _, len)) = &mut in_word {
            *len += 1;
        } else {
            in_word = Some((byte_idx, idx, 1));
        }
    }
    if let Some((start_byte, offset, len)) = in_word {
        let s = syllables(&line.text[start_byte..]);
        words.push((offset, len, s));
    }
    if words.is_empty() {
        // Lines are seconds long, so these fit an f64 exactly.
        #[allow(clippy::cast_precision_loss)]
        let done = position_ms.saturating_sub(line.start_ms) as f64 / span as f64;
        return (done * 100.0).clamp(0.0, 100.0);
    }
    let total: u64 = words.iter().map(|&(_, _, s)| u64::from(s)).sum();
    let sung = span.min(total.saturating_mul(SYLLABLE_MS));
    // Lines and syllable counts are small, so these fit an f64 exactly.
    #[allow(clippy::cast_precision_loss)]
    let (char_pos, chars) = {
        let t = (position_ms.saturating_sub(line.start_ms) as f64 / sung as f64).clamp(0.0, 1.0);
        let w = t * total as f64;
        if w >= total as f64 {
            return 100.0;
        }
        let mut cum = 0.0;
        let mut pos = char_count as f64;
        for &(offset, len, s) in &words {
            let s_f = f64::from(s);
            if w <= cum + s_f {
                let f = (w - cum) / s_f;
                pos = offset as f64 + f * len as f64;
                break;
            }
            cum += s_f;
        }
        (pos, char_count as f64)
    };
    (char_pos / chars * 100.0).clamp(0.0, 100.0)
}

/// [`line_progress`] for a line with word times: the fill runs straight from
/// stamp to stamp, and on from the last one by syllables as if untimed.
fn word_progress(line: &LyricLine, end: u64, position_ms: u64) -> f64 {
    let len = line.text.chars().count();
    if len == 0 {
        return 100.0;
    }
    let mut points = Vec::with_capacity(line.words.len() + 2);
    if line.words.first().map_or(true, |w| w.at != 0) {
        // The server may start a line after its first word so the line above
        // stays selectable; the fill still follows the word.
        let first = line.words.first().map_or(line.start_ms, |w| w.start_ms);
        points.push((line.start_ms.min(first), 0usize));
    }
    for w in &line.words {
        let at = usize::try_from(w.at).unwrap_or(usize::MAX).min(len);
        let prev_time = points.last().map_or(0, |&(t, _)| t);
        points.push((w.start_ms.max(prev_time), at));
    }
    if let Some(&(last_time, last_pos)) = points.last() {
        if last_pos < len {
            let rest_byte = line
                .text
                .char_indices()
                .nth(last_pos)
                .map_or(line.text.len(), |(b, _)| b);
            let rest = &line.text[rest_byte..];
            let tail_syllables = rest.split_whitespace().map(syllables).sum::<u32>().max(1);
            let cap = u64::from(tail_syllables).saturating_mul(SYLLABLE_MS);
            points.push((last_time.saturating_add(cap).min(end).max(last_time), len));
        }
    }
    let idx = points.partition_point(|&(t, _)| t <= position_ms);
    if idx == 0 {
        return 0.0;
    }
    let Some(&(t1, p1)) = points.get(idx) else {
        return 100.0;
    };
    let (t0, p0) = points[idx - 1];
    let dt = t1.saturating_sub(t0);
    // Lines and character counts are small, so these fit an f64 exactly.
    #[allow(clippy::cast_precision_loss)]
    let (pos, chars) = {
        let pos = if dt == 0 {
            p1 as f64
        } else {
            let f = position_ms.saturating_sub(t0) as f64 / dt as f64;
            p0 as f64 + f * (p1 as f64 - p0 as f64)
        };
        (pos, len as f64)
    };
    (pos / chars * 100.0).clamp(0.0, 100.0)
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
    // Outside the panel: its children are the lines, counted by index when
    // the sung line is centred.
    let timing = karaoke && lyrics.words_pending;
    rsx! {
        if timing {
            p {
                class: "pharos-lyrics-timing",
                role: "status",
                "Timing the words… the fill is a guess until then"
            }
        }
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
                if matches!(evt.key(), Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::PageUp) | Key::Named(NamedKey::PageDown)) {
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
                                // Aim one report ahead so the CSS transition lands as
                                // the next position report arrives.
                                style: "--pharos-wipe: {line_progress(&lyrics, i, position_ms.saturating_add(REPORT_MS)):.1}%",
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
    use pharos_client::api::LyricWord;

    fn synced(starts: &[u64]) -> Lyrics {
        Lyrics {
            synced: true,
            lines: starts
                .iter()
                .enumerate()
                .map(|(i, &start_ms)| LyricLine {
                    start_ms,
                    text: format!("line {i}"),
                    words: Vec::new(),
                })
                .collect(),
            words_pending: false,
        }
    }

    fn synced_lines(lines: &[(u64, &str)]) -> Lyrics {
        Lyrics {
            synced: true,
            lines: lines
                .iter()
                .map(|&(start_ms, text)| LyricLine {
                    start_ms,
                    text: text.to_owned(),
                    words: Vec::new(),
                })
                .collect(),
            words_pending: false,
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
        // "line 0" has 2 syllables (sung = 1400 ms) and 6 chars; 700 ms in finishes "line" (4/6).
        assert!((line_progress(&l, 0, 1_700) - 4.0 / 6.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 9_000) - 100.0).abs() < f64::EPSILON);
        // The last line has no next one; 2 syllables cap its fill at 1400 ms (700 ms -> 4/6).
        assert!((line_progress(&l, 2, 5_700) - 4.0 / 6.0 * 100.0).abs() < 1e-9);
        // Two lines at one time: the first is already done.
        assert!((line_progress(&synced(&[0, 0]), 0, 0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn syllables_are_vowel_groups() {
        assert_eq!(syllables("the"), 1);
        assert_eq!(syllables("make"), 1);
        assert_eq!(syllables("table"), 2);
        assert_eq!(syllables("running"), 2);
        assert_eq!(syllables("Honey"), 2);
        assert_eq!(syllables("café"), 2);
        assert_eq!(syllables("résumé"), 3);
        assert_eq!(syllables("ma'am"), 1);
        assert_eq!(syllables("don't"), 1);
        assert_eq!(syllables("make,"), 1);
        assert_eq!(syllables("♪"), 1);
        assert_eq!(syllables("夜空"), 2);
    }

    #[test]
    fn long_words_take_longer_to_fill() {
        let l = synced_lines(&[(0, "a wonderful"), (10_000, "next")]);
        // 1 + 3 = 4 syllables, sung = 2800 ms, 11 chars; 700 ms finishes "a" (1/11).
        assert!((line_progress(&l, 0, 700) - 1.0 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 2_800) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_pause_after_the_line_does_not_slow_the_fill() {
        let l = synced_lines(&[(0, "one two"), (20_000, "next")]);
        // 2 syllables, sung = 1400 ms, 7 chars; 700 ms finishes "one" (3/7).
        assert!((line_progress(&l, 0, 700) - 3.0 / 7.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 1_400) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_line_sung_faster_than_the_cap_fills_to_the_next_line() {
        let l = synced_lines(&[(0, "one two three four"), (2_000, "next")]);
        // 4 syllables * 700 ms = 2800 > 2000, so sung = 2000 ms, 18 chars; 1000 ms finishes "two" (7/18).
        assert!((line_progress(&l, 0, 1_000) - 7.0 / 18.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 2_000) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_long_last_line_fills_over_last_line_ms() {
        let l = synced_lines(&[(0, "one two three four five six seven eight")]);
        // 9 syllables ("seven" is two) * 700 ms = 6300 > 5000, so sung = 5000 ms,
        // 39 chars; 2500 ms is 4.5 syllables, halfway through "five" (21/39).
        assert!((line_progress(&l, 0, 2_500) - 21.0 / 39.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 5_000) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stamped_words_fill_at_their_times() {
        let l = Lyrics {
            synced: true,
            lines: vec![
                LyricLine {
                    start_ms: 1_000,
                    text: "Hello world".into(),
                    words: vec![
                        LyricWord {
                            at: 0,
                            start_ms: 1_000,
                        },
                        LyricWord {
                            at: 6,
                            start_ms: 1_500,
                        },
                        LyricWord {
                            at: 11,
                            start_ms: 2_400,
                        },
                    ],
                },
                LyricLine {
                    start_ms: 5_000,
                    text: "next".into(),
                    words: Vec::new(),
                },
            ],
            words_pending: false,
        };
        assert!((line_progress(&l, 0, 900) - 0.0).abs() < f64::EPSILON);
        assert!((line_progress(&l, 0, 1_250) - 3.0 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 1_500) - 6.0 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 1_950) - 8.5 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 2_400) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_partially_stamped_line_falls_back_to_the_syllable_cap_after_the_last_stamp() {
        let l = Lyrics {
            synced: true,
            lines: vec![
                LyricLine {
                    start_ms: 1_000,
                    text: "Hello world".into(),
                    words: vec![LyricWord {
                        at: 6,
                        start_ms: 1_500,
                    }],
                },
                LyricLine {
                    start_ms: 10_000,
                    text: "next".into(),
                    words: Vec::new(),
                },
            ],
            words_pending: false,
        };
        // No stamp at 0, so (1000, 0) -> (1500, 6); "world" (1 syllable = 700 ms) ends at 2200.
        assert!((line_progress(&l, 0, 1_250) - 3.0 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 1_850) - 8.5 / 11.0 * 100.0).abs() < 1e-9);
        assert!((line_progress(&l, 0, 2_200) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_prefixed_line_starting_after_its_first_word_fills_at_the_word_time() {
        let l = Lyrics {
            synced: true,
            lines: vec![
                LyricLine {
                    start_ms: 12_001,
                    text: "♪ hello".into(),
                    words: vec![LyricWord {
                        at: 2,
                        start_ms: 11_000,
                    }],
                },
                LyricLine {
                    start_ms: 20_000,
                    text: "next".into(),
                    words: Vec::new(),
                },
            ],
            words_pending: false,
        };
        // (11000, 0) -> (11000, 2) -> "hello" (2 syllables, 1400 ms) ends at 12400.
        assert!((line_progress(&l, 0, 11_700) - 4.5 / 7.0 * 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_word_stamp_past_the_last_line_end_keeps_points_ordered() {
        let l = Lyrics {
            synced: true,
            lines: vec![LyricLine {
                start_ms: 0,
                text: "Hello world".into(),
                words: vec![LyricWord {
                    at: 0,
                    start_ms: 6_000,
                }],
            }],
            words_pending: false,
        };
        assert!((line_progress(&l, 0, 5_500) - 0.0).abs() < f64::EPSILON);
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

    /// B1034: the TV remote passes over the lyrics lines by their class.
    #[test]
    fn the_tv_remote_skips_name_the_lyrics_lines() {
        let html = render(synced(&[1_000, 3_000]), 1_500);
        let class = super::super::tv_page::REMOTE_SKIPS.trim_start_matches('.');
        assert!(html.contains(&format!(r#"class="{class}""#)), "{html}");
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
                words: Vec::new(),
            })
            .collect();
        let html = render(
            Lyrics {
                synced: false,
                lines,
                words_pending: false,
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
        // 1800 ms + 250 ms report lead = 1050 ms into "line 0" (sung = 1400 ms):
        // w = 1.5 syllables -> halfway through "0" at char 5.5 of 6 = 91.7%.
        let html = render_as(l.clone(), 1_800, true);
        assert!(html.contains(r#"data-karaoke="true""#), "{html}");
        assert_eq!(html.matches("pharos-lyrics-words").count(), 1, "{html}");
        assert!(html.contains("--pharos-wipe: 91.7%"), "{html}");
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
    fn karaoke_says_so_while_the_words_are_still_being_timed() {
        let mut l = synced(&[1_000, 3_000]);
        l.words_pending = true;
        let timing = render_as(l.clone(), 1_500, true);
        assert_eq!(
            timing.matches("pharos-lyrics-timing").count(),
            1,
            "{timing}"
        );
        assert!(timing.contains(r#"role="status""#), "{timing}");
        // Lines only, no karaoke: the panel draws no fill to apologise for.
        let lines_only = render_as(l.clone(), 1_500, false);
        assert!(!lines_only.contains("pharos-lyrics-timing"), "{lines_only}");
        l.words_pending = false;
        let timed = render_as(l, 1_500, true);
        assert!(!timed.contains("pharos-lyrics-timing"), "{timed}");
    }

    #[test]
    fn an_empty_synced_line_shows_as_an_instrumental_gap() {
        let mut l = synced(&[0, 2_000]);
        l.lines[1].text = String::new();
        let html = render(l, 0);
        assert!(html.contains("pharos-lyrics-gap"), "{html}");
    }
}
