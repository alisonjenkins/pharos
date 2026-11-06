//! Turning visible source text into safe ASS event text.

use crate::source::{self, SourceLine};

/// Escapes visible text for an ASS Text field: no braces (they would open an
/// override block), no backslashes (`\N` and friends), no control characters.
/// A `'\n'` becomes the ASS hard break.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '{' | '}' => {}
            '\\' => out.push('/'),
            '\n' => out.push_str("\\N"),
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The line's text with its italics kept as `{\i1}` / `{\i0}`.
pub(crate) fn body(line: &SourceLine) -> String {
    let mut out = String::new();
    let mut on = false;
    for run in source::runs(line) {
        if run.italic != on {
            out.push_str(if run.italic { "{\\i1}" } else { "{\\i0}" });
            on = run.italic;
        }
        out.push_str(&esc(&run.text));
    }
    if on {
        out.push_str("{\\i0}");
    }
    out
}

/// The line's text without any italics, escaped; for effects whose style
/// decides the slant.
pub(crate) fn flat(line: &SourceLine) -> String {
    esc(&line.text)
}

/// Drops one pair of wrapping marks (`(..)`, `[..]`, `*..*`) from `s`.
pub(crate) fn unwrap_marks(s: &str) -> &str {
    let t = s.trim();
    for (open, close) in [('(', ')'), ('（', '）'), ('[', ']'), ('*', '*')] {
        if let Some(inner) = t.strip_prefix(open).and_then(|r| r.strip_suffix(close)) {
            if !inner.contains(open) && !inner.contains(close) {
                return inner.trim();
            }
        }
    }
    t
}

/// The line's text with wrapping marks dropped, escaped.
pub(crate) fn flat_unwrapped(line: &SourceLine) -> String {
    esc(unwrap_marks(&line.text))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn escapes_braces_backslashes_and_breaks() {
        assert_eq!(esc("a{b}c\\d\ne\tf"), "abc/d\\Ne f");
    }

    #[test]
    fn unwraps_one_pair() {
        assert_eq!(unwrap_marks(" (thinking) "), "thinking");
        assert_eq!(unwrap_marks("[boom]"), "boom");
        assert_eq!(unwrap_marks("*bang*"), "bang");
        assert_eq!(unwrap_marks("(a) and (b)"), "(a) and (b)");
        assert_eq!(unwrap_marks("plain"), "plain");
    }
}
