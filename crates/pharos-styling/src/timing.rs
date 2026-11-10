//! Fill timings for sung lines whose source has no `\k` (B1212): from the
//! aligner's word stamps when the words are what is sung, else paced evenly
//! over the line.

use crate::signals::SungLine;
use crate::source::{SourceLine, Syllable};

/// Share of a line's time a paced fill takes: the singing usually ends a
/// little before the cue does.
const PACED_SHARE: f32 = 0.9;

/// One syllable per word, each as long as its share of the line's vowel
/// groups, the whole fill over [`PACED_SHARE`] of the line. `None` for a
/// line with no letters.
pub(crate) fn paced(line: &SourceLine) -> Option<Vec<Syllable>> {
    let words = words(&line.text);
    let weights: Vec<u32> = words.iter().map(|w| vowel_groups(w)).collect();
    let total: u32 = weights.iter().sum();
    if total == 0 {
        return None;
    }
    let span = (line.end_ms - line.start_ms).max(0) as f32 * PACED_SHARE;
    let mut out = Vec::with_capacity(words.len());
    let mut given = 0u32;
    let mut weight_so_far = 0u32;
    for (w, weight) in words.iter().zip(&weights) {
        weight_so_far += weight;
        // Cumulative rounding, so the parts add up to the whole.
        let until = (span * weight_so_far as f32 / total as f32).round() as u32;
        out.push(Syllable {
            dur_ms: until.saturating_sub(given),
            text: (*w).to_owned(),
        });
        given = until;
    }
    Some(out)
}

/// Syllables from the aligner's stamps: the text before the first word is
/// held until it is sung, then each word fills until the next starts.
/// `None` when the stamps do not fit the line's text or times.
pub(crate) fn aligned(line: &SourceLine, sung: &SungLine) -> Option<Vec<Syllable>> {
    let chars: Vec<char> = line.text.chars().collect();
    let stamps = &sung.words;
    if stamps.len() < 2 {
        return None;
    }
    let ordered = stamps
        .windows(2)
        .all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1);
    let (last_at, _) = stamps[stamps.len() - 1];
    let (_, first_ms) = stamps[0];
    if !ordered || last_at != chars.len() || first_ms < line.start_ms {
        return None;
    }
    let slice = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let ms = |d: i64| u32::try_from(d).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(stamps.len());
    out.push(Syllable {
        dur_ms: ms(first_ms - line.start_ms),
        text: slice(0, stamps[0].0),
    });
    for w in stamps.windows(2) {
        out.push(Syllable {
            dur_ms: ms(w[1].1 - w[0].1),
            text: slice(w[0].0, w[1].0),
        });
    }
    Some(out)
}

/// `text` cut after each run of whitespace, so the pieces join back to it.
fn words(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_space = true;
        } else if in_space {
            out.push(&text[start..i]);
            start = i;
            in_space = false;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Runs of vowels, at least one for a word with any letter.
fn vowel_groups(word: &str) -> u32 {
    let mut groups = 0;
    let mut prev = false;
    for c in word.chars() {
        let v = matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
        if v && !prev {
            groups += 1;
        }
        prev = v;
    }
    if groups == 0 && word.chars().any(char::is_alphabetic) {
        1
    } else {
        groups
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::effects::testutil;

    fn joined(s: &[Syllable]) -> String {
        s.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn a_paced_fill_covers_the_text_and_most_of_the_line() {
        // testutil lines run 10.000 s to 13.000 s.
        let line = testutil::line("Hold on to\nthe light");
        let s = paced(&line).unwrap();
        assert_eq!(joined(&s), line.text);
        assert_eq!(s.iter().map(|s| s.dur_ms).sum::<u32>(), 2700);
        // "Hold", "on", "to", "the", "light": one vowel group each.
        assert!(s.iter().all(|s| s.dur_ms == 540), "{s:?}");
        assert_eq!(paced(&testutil::line("...!")), None);
    }

    #[test]
    fn aligned_stamps_become_word_fills() {
        let line = testutil::line("♪ Run away");
        let sung = SungLine {
            words: vec![(2, 10_400), (6, 10_900), (10, 11_800)],
            score: Some(0.7),
        };
        let s = aligned(&line, &sung).unwrap();
        let got: Vec<(u32, &str)> = s.iter().map(|s| (s.dur_ms, s.text.as_str())).collect();
        assert_eq!(got, [(400, "♪ "), (500, "Run "), (900, "away")]);
    }

    #[test]
    fn stamps_that_do_not_fit_the_line_are_refused() {
        let line = testutil::line("Run away");
        let bad = |words: Vec<(usize, i64)>| SungLine { words, score: None };
        // Out of order, short of the text, before the line, or too few.
        for sung in [
            bad(vec![(0, 10_900), (4, 10_400), (8, 11_000)]),
            bad(vec![(0, 10_100), (4, 10_400)]),
            bad(vec![(0, 9_000), (4, 10_400), (8, 11_000)]),
            bad(vec![(8, 11_000)]),
        ] {
            assert_eq!(aligned(&line, &sung), None, "{sung:?}");
        }
    }
}
