//! Stand-ins for CJK punctuation in lines that are otherwise not CJK
//! (B1209).
//!
//! Translated signs often keep the source's `【Name】` brackets. The fonts
//! a track brings for Latin text rarely have those glyphs, and the browser
//! renderer has no system fallback, so they drew as empty boxes. A line
//! with no kana or kanji gets the nearest Latin mark instead; a line that
//! does have them is left alone, since its font must already carry them.

use crate::tags::{self, Piece};
use std::borrow::Cow;

/// `text` (an ASS event's text, override blocks included) with CJK
/// punctuation outside blocks and drawings swapped for Latin marks, when
/// nothing else in it is CJK.
pub(crate) fn latin_punctuation(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| stand_in(c).is_some()) || has_cjk_letters(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut drawing = false;
    for piece in tags::pieces(text) {
        match piece {
            Piece::Block(b) => {
                for tag in tags::scan(b) {
                    if tag.name == "p" && !tag.nested {
                        let digits: String =
                            tag.arg.chars().take_while(char::is_ascii_digit).collect();
                        drawing = digits.parse::<u32>().is_ok_and(|n| n > 0);
                    }
                }
                out.push('{');
                out.push_str(b);
                out.push('}');
            }
            Piece::Text(t) if drawing => out.push_str(t),
            Piece::Text(t) => {
                for c in t.chars() {
                    match stand_in(c) {
                        Some(s) => out.push_str(s),
                        None => out.push(c),
                    }
                }
            }
        }
    }
    Cow::Owned(out)
}

fn stand_in(c: char) -> Option<&'static str> {
    Some(match c {
        '【' | '〔' | '［' | '〖' => "[",
        '】' | '〕' | '］' | '〗' => "]",
        '「' | '『' => "\u{201C}",
        '」' | '』' => "\u{201D}",
        '〈' => "\u{2039}",
        '〉' => "\u{203A}",
        '《' => "\u{AB}",
        '》' => "\u{BB}",
        '（' => "(",
        '）' => ")",
        '・' => "\u{B7}",
        '、' => ",",
        '。' => ".",
        '！' => "!",
        '？' => "?",
        '：' => ":",
        '～' => "~",
        '\u{3000}' => " ",
        _ => return None,
    })
}

/// Kana, kanji or hangul anywhere in `text`.
fn has_cjk_letters(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{30FF}' // hiragana, katakana (・ is punctuation, below)
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{FF66}'..='\u{FF9F}')
            && c != '・'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_in_a_latin_line_become_latin() {
        assert_eq!(
            latin_punctuation(r"{\c&HB98B75&}【Name】 {\c&HD2D2D2&}Alex"),
            r"{\c&HB98B75&}[Name] {\c&HD2D2D2&}Alex"
        );
        assert_eq!(
            latin_punctuation("「Hello」・World。"),
            "\u{201C}Hello\u{201D}\u{B7}World."
        );
    }

    #[test]
    fn a_line_with_kana_or_kanji_keeps_its_punctuation() {
        for s in ["【名前】", "「ありがとう」", "【Note】 名前"] {
            assert_eq!(latin_punctuation(s), s);
        }
    }

    #[test]
    fn blocks_and_drawings_are_untouched() {
        let s = r"{\p1}m 0 0 l 1 1{\p0}【A】";
        assert_eq!(latin_punctuation(s), r"{\p1}m 0 0 l 1 1{\p0}[A]");
        assert!(matches!(latin_punctuation("plain"), Cow::Borrowed(_)));
    }
}
