//! How much of a song's lyric text was heard (B1479/V1468): the share of
//! the lyric's word pairs that the recogniser's transcript also holds. The
//! aligner judges its own lyrics with it, and the server scores other
//! online records against the same transcript, so both read one measure.

use std::collections::HashSet;

/// The share of the lyric's word pairs found in `heard`, the transcript's
/// text joined as the recogniser wrote it, from 0 to 1, or
/// `None` when the lyric cannot be judged this way: it has no pair of
/// words in one line, or under half its letters are ASCII (a script whose
/// words are not split by spaces, or spelt differently by the recogniser).
///
/// Pairs are taken within each lyric line, since a line break is not a
/// gap the singer leaves out, and across the whole transcript, whose
/// segments split where the recogniser's windows fell. A pair counts once
/// for each time the lyric holds it.
pub fn bigram_recall<'a>(
    lyric_lines: impl IntoIterator<Item = &'a str>,
    heard: &str,
) -> Option<f64> {
    let lines: Vec<&str> = lyric_lines.into_iter().collect();
    if !mostly_ascii(&lines) {
        return None;
    }
    let mut wanted = Vec::new();
    for line in &lines {
        let words = words(line);
        wanted.extend(words.windows(2).map(|w| (w[0].clone(), w[1].clone())));
    }
    if wanted.is_empty() {
        return None;
    }
    let heard = words(heard);
    let pairs: HashSet<(&str, &str)> = heard
        .windows(2)
        .map(|w| (w[0].as_str(), w[1].as_str()))
        .collect();
    let found = wanted
        .iter()
        .filter(|(a, b)| pairs.contains(&(a.as_str(), b.as_str())))
        .count();
    Some(found as f64 / wanted.len() as f64)
}

/// The words of `text`, lowercased: letters and digits only, an apostrophe
/// dropped inside a word ("you're" is "youre", as the recogniser may write
/// it either way), anything else a break.
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if c == '\'' || c == '\u{2019}' {
            continue;
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn mostly_ascii(lines: &[&str]) -> bool {
    let (ascii, all) = lines
        .iter()
        .flat_map(|l| l.chars())
        .filter(|c| c.is_alphabetic())
        .fold((0usize, 0usize), |(a, n), c| {
            (a + usize::from(c.is_ascii()), n + 1)
        });
    all > 0 && ascii * 2 >= all
}

#[cfg(test)]
mod tests {
    use super::*;

    const LYRIC: [&str; 3] = [
        "grey gulls wheel above the quay",
        "low tide hums along the stones",
        "we're walking home",
    ];

    fn recall(lyric: &[&str], transcript: &str) -> Option<f64> {
        bigram_recall(lyric.iter().copied(), transcript)
    }

    #[test]
    fn the_lyric_heard_whole_scores_one() {
        let got = recall(
            &LYRIC,
            "Grey gulls wheel above the quay, low tide hums along the stones. Were walking home.",
        );
        assert_eq!(got, Some(1.0));
    }

    #[test]
    fn a_different_lyric_scores_near_zero() {
        let got = recall(
            &LYRIC,
            "the lanterns swing in the orchard while the river keeps its own slow time",
        );
        assert!(matches!(got, Some(r) if r < 0.1), "{got:?}");
    }

    #[test]
    fn half_the_pairs_heard_scores_a_half() {
        let got = recall(&["one two three", "four five six"], "one two three");
        assert_eq!(got, Some(0.5));
    }

    #[test]
    fn a_repeated_pair_counts_each_time_the_lyric_holds_it() {
        let got = recall(&["la la la", "go home"], "la la");
        assert_eq!(got, Some(2.0 / 3.0));
    }

    #[test]
    fn pairs_do_not_span_lyric_lines() {
        let got = recall(&["one two", "three four"], "one two four");
        assert_eq!(got, Some(0.5));
    }

    #[test]
    fn a_lyric_with_no_pair_of_words_is_not_judged() {
        assert_eq!(recall(&["oh", "yeah"], "oh yeah"), None);
        assert_eq!(recall(&[], "oh yeah"), None);
    }

    #[test]
    fn a_lyric_mostly_outside_ascii_is_not_judged() {
        assert_eq!(
            recall(&["\u{6d77} \u{98a8}", "\u{7a7a} \u{96f2}"], "nothing"),
            None
        );
        assert!(recall(&["caf\u{e9} au lait", "s'il vous pla\u{ee}t"], "cafe").is_some());
    }

    #[test]
    fn words_drop_apostrophes_and_split_on_punctuation() {
        assert_eq!(words("You're—here, ok?"), ["youre", "here", "ok"]);
        assert_eq!(
            words("rock\u{2019}n\u{2019}roll 4ever"),
            ["rocknroll", "4ever"]
        );
    }
}
