//! Lyric text to alignable words, as `pharos_aligner.text.words` does it.

use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// A word of a lyric line: where it starts and the letters the model can align.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Offset of the word's first character in the line, counted in Unicode
    /// code points (Python string indices), not bytes.
    pub at: usize,
    /// Lowercase letters from the model's alphabet: `a`-`z` and `'`.
    pub token: String,
}

/// Python's `\s` for str patterns: `str.isspace`, which is Rust's White_Space
/// plus the four ASCII information separators U+001C..U+001F.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn in_alphabet(c: char) -> bool {
    c.is_ascii_lowercase() || c == '\''
}

/// Splits on whitespace, folds accents (NFKD, combining marks dropped),
/// lowercases and keeps only alphabet characters; words left empty are skipped.
pub fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut chars = text.chars().enumerate().peekable();
    while let Some((at, c)) = chars.next() {
        if is_py_space(c) {
            continue;
        }
        let mut raw = String::from(c);
        while let Some(&(_, next)) = chars.peek() {
            if is_py_space(next) {
                break;
            }
            raw.push(next);
            chars.next();
        }
        let token = fold(&raw);
        if !token.is_empty() {
            out.push(Word { at, token });
        }
    }
    out
}

fn fold(raw: &str) -> String {
    raw.chars()
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .nfkd()
        .filter(|&c| canonical_combining_class(c) == 0)
        .flat_map(char::to_lowercase)
        .filter(|&c| in_alphabet(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(usize, String)> {
        words(text).into_iter().map(|w| (w.at, w.token)).collect()
    }

    fn want(pairs: &[(usize, &str)]) -> Vec<(usize, String)> {
        pairs.iter().map(|&(at, t)| (at, t.to_string())).collect()
    }

    #[test]
    fn offsets_count_code_points_not_bytes() {
        assert_eq!(tokens("\u{e9}t\u{e9} ici"), want(&[(0, "ete"), (4, "ici")]));
    }

    #[test]
    fn curly_apostrophe_becomes_straight() {
        assert_eq!(tokens("don\u{2019}t"), want(&[(0, "don't")]));
    }

    #[test]
    fn words_with_no_alphabet_letters_are_dropped() {
        assert_eq!(tokens("12 -- ab"), want(&[(6, "ab")]));
    }
}
