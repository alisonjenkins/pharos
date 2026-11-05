//! LRC text to lyric lines. Text with no timestamped line is plain lyrics,
//! one line per line, so the same parser serves `.lrc`, `.txt` and tag text.

use super::{LyricLine, WordStamp};

/// Parse lyrics text. Timestamped text gives synced lines sorted by start;
/// otherwise each line is kept as written, minus blank lines at either end.
pub fn parse(text: &str) -> Vec<LyricLine> {
    let mut offset_ms: i64 = 0;
    let mut timed: Vec<(u64, String, Vec<WordStamp>)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(ms) = offset_tag(line) {
            offset_ms = ms;
            continue;
        }
        let (stamps, raw_words) = stamps(line);
        if stamps.is_empty() {
            continue;
        }
        let (cleaned, word_times) = word_stamps(raw_words);
        // Word times are absolute, so a line repeated under several `[mm:ss]`
        // stamps keeps none.
        let keep_words = stamps.len() == 1;
        for at in stamps {
            let valid_words = keep_words && word_times.first().map_or(true, |w| w.start_ms >= at);
            timed.push((
                at,
                cleaned.clone(),
                if valid_words {
                    word_times.clone()
                } else {
                    Vec::new()
                },
            ));
        }
    }
    if timed.is_empty() {
        return plain(text);
    }
    // LRC's offset moves every line earlier when positive.
    let mut lines: Vec<LyricLine> = timed
        .into_iter()
        .map(|(at, text, words)| LyricLine {
            start_ms: Some(shift(at, offset_ms)),
            text,
            words: words
                .into_iter()
                .map(|w| WordStamp {
                    at: w.at,
                    start_ms: shift(w.start_ms, offset_ms),
                })
                .collect(),
        })
        .collect();
    lines.sort_by_key(|l| l.start_ms);
    lines
}

fn plain(text: &str) -> Vec<LyricLine> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !is_id_tag(l.trim()))
        .collect();
    let first = lines.iter().position(|l| !l.trim().is_empty());
    let last = lines.iter().rposition(|l| !l.trim().is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return Vec::new();
    };
    lines
        .get(first..=last)
        .unwrap_or_default()
        .iter()
        .map(|l| LyricLine {
            start_ms: None,
            text: (*l).to_owned(),
            words: Vec::new(),
        })
        .collect()
}

fn shift(at: u64, offset_ms: i64) -> u64 {
    if offset_ms >= 0 {
        at.saturating_sub(offset_ms.unsigned_abs())
    } else {
        at.saturating_add(offset_ms.unsigned_abs())
    }
}

/// The LRC header tags; a bracketed line with any other key, such as
/// `[Chorus: Band]`, is a lyric.
const ID_TAGS: [&str; 10] = [
    "ar", "al", "ti", "au", "by", "length", "offset", "re", "ve", "tool",
];

/// An LRC header such as `[ar:Band]` or `[offset:+250]`.
fn is_id_tag(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|l| l.strip_suffix(']'))
        .and_then(|inner| inner.split_once(':'))
        .is_some_and(|(key, _)| ID_TAGS.iter().any(|t| key.trim().eq_ignore_ascii_case(t)))
}

/// `[offset:+250]` → `Some(250)`.
fn offset_tag(line: &str) -> Option<i64> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (key, value) = inner.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("offset") {
        return None;
    }
    let value = value.trim();
    value.strip_prefix('+').unwrap_or(value).parse().ok()
}

/// The leading `[mm:ss.xx]` stamps of a line and the words after them. A line
/// may carry several stamps when a chorus repeats.
fn stamps(line: &str) -> (Vec<u64>, &str) {
    let mut rest = line;
    let mut out = Vec::new();
    while let Some(after) = rest.strip_prefix('[') {
        let Some((inner, tail)) = after.split_once(']') else {
            break;
        };
        let Some(ms) = timestamp_ms(inner) else {
            break;
        };
        out.push(ms);
        rest = tail;
    }
    (out, rest.trim())
}

/// `mm:ss`, `mm:ss.f`…`mm:ss.fff`, or `mm:ss:ff` in milliseconds.
fn timestamp_ms(stamp: &str) -> Option<u64> {
    let (min, rest) = stamp.split_once(':')?;
    let (sec, frac) = match rest.split_once(['.', ':']) {
        Some((sec, frac)) => (sec, Some(frac)),
        None => (rest, None),
    };
    if !all_digits(min) || !all_digits(sec) {
        return None;
    }
    let min: u64 = min.parse().ok()?;
    let sec: u64 = sec.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    let frac_ms = match frac {
        None => 0,
        Some(f) if all_digits(f) && f.len() <= 3 => {
            let n: u64 = f.parse().ok()?;
            match f.len() {
                1 => n * 100,
                2 => n * 10,
                _ => n,
            }
        }
        Some(_) => return None,
    };
    min.checked_mul(60_000)?
        .checked_add(sec * 1_000)?
        .checked_add(frac_ms)
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Pull enhanced-LRC word stamps (`<00:12.30>`) from a line's words,
/// returning the whitespace-normalised text and the stamps at their char
/// offsets in that text. Decreasing stamps are dropped.
fn word_stamps(words: &str) -> (String, Vec<WordStamp>) {
    let mut out = String::with_capacity(words.len());
    let mut chars = 0u32;
    let mut in_word = false;
    let mut pending = Vec::<u64>::new();
    let mut stamps = Vec::<WordStamp>::new();
    let mut push_text = |piece: &str, pending: &mut Vec<u64>, stamps: &mut Vec<WordStamp>| {
        for ch in piece.chars() {
            if ch.is_whitespace() {
                if in_word {
                    for start_ms in pending.drain(..) {
                        stamps.push(WordStamp {
                            at: chars,
                            start_ms,
                        });
                    }
                    in_word = false;
                }
            } else {
                if !in_word && chars > 0 {
                    out.push(' ');
                    chars = chars.saturating_add(1);
                }
                for start_ms in pending.drain(..) {
                    stamps.push(WordStamp {
                        at: chars,
                        start_ms,
                    });
                }
                out.push(ch);
                chars = chars.saturating_add(1);
                in_word = true;
            }
        }
    };
    let mut rest = words;
    while let Some(open) = rest.find('<') {
        let (before, after) = rest.split_at(open);
        push_text(before, &mut pending, &mut stamps);
        let stamp = after
            .get(1..)
            .and_then(|a| a.split_once('>'))
            .and_then(|(inner, tail)| timestamp_ms(inner).map(|ms| (ms, tail)));
        match stamp {
            Some((ms, tail)) => {
                pending.push(ms);
                rest = tail;
            }
            None => {
                push_text("<", &mut pending, &mut stamps);
                rest = after.get(1..).unwrap_or_default();
            }
        }
    }
    push_text(rest, &mut pending, &mut stamps);
    for start_ms in pending {
        stamps.push(WordStamp {
            at: chars,
            start_ms,
        });
    }
    if out.is_empty() || stamps.windows(2).any(|w| w[0].start_ms > w[1].start_ms) {
        stamps.clear();
    }
    (out, stamps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(lines: &[LyricLine]) -> Vec<(Option<u64>, &str)> {
        lines
            .iter()
            .map(|l| (l.start_ms, l.text.as_str()))
            .collect()
    }

    #[test]
    fn synced_lines_carry_their_start_and_skip_tags() {
        let text = "[ar:Band]\n[ti:Song]\n[00:01.50]One\n[00:03.25]Two\n\n[01:02]Three\n";
        assert_eq!(
            starts(&parse(text)),
            vec![
                (Some(1_500), "One"),
                (Some(3_250), "Two"),
                (Some(62_000), "Three")
            ]
        );
    }

    #[test]
    fn fraction_widths_and_a_colon_separator_all_mean_milliseconds() {
        let text = "[00:01.5]a\n[00:02.50]b\n[00:03.500]c\n[00:04:50]d";
        let got: Vec<_> = parse(text).iter().map(|l| l.start_ms).collect();
        assert_eq!(
            got,
            vec![Some(1_500), Some(2_500), Some(3_500), Some(4_500)]
        );
    }

    #[test]
    fn a_repeated_chorus_line_appears_at_each_of_its_stamps_in_order() {
        let text = "[00:10.00][00:30.00]Chorus\n[00:20.00]Verse";
        assert_eq!(
            starts(&parse(text)),
            vec![
                (Some(10_000), "Chorus"),
                (Some(20_000), "Verse"),
                (Some(30_000), "Chorus")
            ]
        );
    }

    #[test]
    fn offset_moves_lines_earlier_when_positive_and_never_below_zero() {
        let text = "[offset:+500]\n[00:00.20]a\n[00:02.00]b";
        let got: Vec<_> = parse(text).iter().map(|l| l.start_ms).collect();
        assert_eq!(got, vec![Some(0), Some(1_500)]);
        let later = parse("[offset:-500]\n[00:01.00]a");
        assert_eq!(later.first().and_then(|l| l.start_ms), Some(1_500));
    }

    #[test]
    fn word_stamps_are_kept_with_their_cleaned_char_offsets() {
        let lines = parse("[00:12.00]<00:12.00>Hello <00:12.50>world<00:13.40>");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(12_000),
                text: "Hello world".into(),
                words: vec![
                    WordStamp {
                        at: 0,
                        start_ms: 12_000
                    },
                    WordStamp {
                        at: 6,
                        start_ms: 12_500
                    },
                    WordStamp {
                        at: 11,
                        start_ms: 13_400
                    },
                ],
            }]
        );
    }

    #[test]
    fn word_stamps_with_extra_spaces_map_to_cleaned_offsets() {
        let lines = parse("[00:01.00]  <00:01.00>  Hello   <00:01.50>   world  <00:02.00>  ");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "Hello world".into(),
                words: vec![
                    WordStamp {
                        at: 0,
                        start_ms: 1_000
                    },
                    WordStamp {
                        at: 6,
                        start_ms: 1_500
                    },
                    WordStamp {
                        at: 11,
                        start_ms: 2_000
                    },
                ],
            }]
        );
    }

    #[test]
    fn offset_shifts_word_stamps_like_line_stamps() {
        let lines = parse("[offset:+500]\n[00:01.00]<00:01.00>Hello <00:01.50>world<00:02.00>");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(500),
                text: "Hello world".into(),
                words: vec![
                    WordStamp {
                        at: 0,
                        start_ms: 500
                    },
                    WordStamp {
                        at: 6,
                        start_ms: 1_000
                    },
                    WordStamp {
                        at: 11,
                        start_ms: 1_500
                    },
                ],
            }]
        );
    }

    #[test]
    fn a_line_with_multiple_line_stamps_keeps_no_word_stamps() {
        let lines = parse("[00:10.00][00:30.00]<00:10.00>Chorus <00:10.50>line");
        assert_eq!(
            starts(&lines),
            vec![(Some(10_000), "Chorus line"), (Some(30_000), "Chorus line")]
        );
        assert!(lines.iter().all(|l| l.words.is_empty()));
    }

    #[test]
    fn decreasing_word_stamps_are_dropped() {
        let lines = parse("[00:01.00]<00:01.50>Hello <00:01.20>world");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "Hello world".into(),
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn word_stamps_earlier_than_their_line_stamp_are_dropped() {
        let lines = parse("[00:10.00]<00:01.00>Hello <00:01.50>world");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(10_000),
                text: "Hello world".into(),
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_word_end_stamp_before_whitespace_marks_the_end_of_that_word() {
        let lines = parse("[00:01.00]<00:01.00>Hello<00:01.40> <00:01.80>world<00:02.20>");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "Hello world".into(),
                words: vec![
                    WordStamp {
                        at: 0,
                        start_ms: 1_000
                    },
                    WordStamp {
                        at: 5,
                        start_ms: 1_400
                    },
                    WordStamp {
                        at: 6,
                        start_ms: 1_800
                    },
                    WordStamp {
                        at: 11,
                        start_ms: 2_200
                    },
                ],
            }]
        );
    }

    #[test]
    fn a_line_with_only_a_word_stamp_and_no_text_keeps_no_words() {
        let lines = parse("[00:02.00]<00:02.00>");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(2_000),
                text: String::new(),
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_line_without_word_stamps_has_empty_words() {
        let lines = parse("[00:01.00]Hello world");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "Hello world".into(),
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn non_stamp_angle_brackets_are_kept_in_the_text() {
        let lines = parse("[00:01.00]<00:01.00>Hello <00:01.50>world <3");
        assert_eq!(
            lines,
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "Hello world <3".into(),
                words: vec![
                    WordStamp {
                        at: 0,
                        start_ms: 1_000
                    },
                    WordStamp {
                        at: 6,
                        start_ms: 1_500
                    },
                ],
            }]
        );
    }

    #[test]
    fn untimed_text_is_plain_and_keeps_stanza_breaks() {
        let lines = parse("\n\nFirst line\nSecond line\n\nNext verse\n\n");
        assert_eq!(
            starts(&lines),
            vec![
                (None, "First line"),
                (None, "Second line"),
                (None, ""),
                (None, "Next verse")
            ]
        );
    }

    #[test]
    fn plain_text_drops_lrc_headers() {
        let lines = parse("[ar:Band]\n[length: 3:20]\nWords");
        assert_eq!(starts(&lines), vec![(None, "Words")]);
    }

    #[test]
    fn a_section_header_naming_a_singer_is_kept() {
        let lines = parse("[Chorus: Band]\nSing it");
        assert_eq!(
            starts(&lines),
            vec![(None, "[Chorus: Band]"), (None, "Sing it")]
        );
    }

    #[test]
    fn a_bracketed_word_is_not_a_timestamp() {
        let lines = parse("[Chorus]\nSing it\n[99:99]x");
        assert!(lines.iter().all(|l| l.start_ms.is_none()));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn empty_text_has_no_lines() {
        assert!(parse("").is_empty());
        assert!(parse("\n  \n").is_empty());
    }
}
