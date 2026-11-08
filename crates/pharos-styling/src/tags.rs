//! A scanner for ASS override blocks, shared by the source reader (what does
//! this event carry?) and the budget checker (what does this output spend?).

/// One override tag, e.g. `\fad(200,0)` is name `fad`, arg `200,0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tag {
    pub name: String,
    pub arg: String,
    /// Found inside a `\t(...)` animation rather than at the block's top level.
    pub nested: bool,
}

/// The tags whose argument is a size in script pixels, longest first so
/// `\xbord` is not read as `\bord`.
const SIZED: &[&str] = &[
    "xbord", "ybord", "xshad", "yshad", "bord", "shad", "blur", "fs",
];

/// `text` with the argument of every size tag in its override blocks
/// (`\fs`, `\bord`, `\shad`, `\blur` and their axis forms, nested in
/// `\t(...)` too) multiplied by `k`, so a line's inline sizes follow the
/// viewer's subtitle size. Text outside `{...}` is untouched.
pub(crate) fn scale_sizes(text: &str, k: f32) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            '{' => in_block = true,
            '}' => in_block = false,
            '\\' if in_block => {
                out.push(c);
                if let Some(name) = SIZED.iter().find(|n| rest.starts_with(**n)) {
                    let after = &rest[name.len()..];
                    let len = after
                        .bytes()
                        .take_while(|b| b.is_ascii_digit() || *b == b'.')
                        .count();
                    if let Some(v) = after.get(..len).and_then(|n| n.parse::<f32>().ok()) {
                        out.push_str(name);
                        out.push_str(&size_arg(v * k));
                        rest = &after[len..];
                    }
                }
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// A size argument with at most two decimals and no trailing zeros.
fn size_arg(v: f32) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Every tag name the scanner can split off a run of letters. Names outside
/// the FR-004 allow-list are listed too so `\alpha` is never read as `\a`.
const KNOWN: &[&str] = &[
    "t", "move", "fad", "fade", "k", "kf", "K", "ko", "frx", "fry", "frz", "fscx", "fscy", "fsp",
    "blur", "be", "bord", "shad", "clip", "iclip", "p", "org", "pos", "an", "c", "alpha", "fn",
    "fs", "b", "i", "r", "xbord", "ybord", "xshad", "yshad", "fax", "fay", "fe", "fr", "q", "a",
    "u", "s", "pbo", "kt",
];

/// Splits the text of an event into alternating plain text and `{...}` blocks.
/// A `{` with no closing `}` is plain text, as libass reads it.
pub(crate) enum Piece<'a> {
    Text(&'a str),
    Block(&'a str),
}

pub(crate) fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let Some(close_rel) = rest[open + 1..].find('}') else {
            break;
        };
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        out.push(Piece::Block(&rest[open + 1..open + 1 + close_rel]));
        rest = &rest[open + 1 + close_rel + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    out
}

/// All tags of one block, nested `\t(...)` contents flattened after their `\t`.
pub(crate) fn scan(block: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    scan_into(block, false, &mut out);
    out
}

/// All tags of every override block in an event's text.
pub(crate) fn scan_text(text: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    for p in pieces(text) {
        if let Piece::Block(b) = p {
            scan_into(b, false, &mut out);
        }
    }
    out
}

fn scan_into(block: &str, nested: bool, out: &mut Vec<Tag>) {
    let b = block.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        i += 1;
        let name_len = name_len(&b[i..]);
        if name_len == 0 {
            continue;
        }
        let name = &block[i..i + name_len];
        i += name_len;
        let arg_start = i;
        let arg;
        if b.get(i) == Some(&b'(') {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            arg = &block[arg_start + 1..j.min(b.len())];
            i = (j + 1).min(b.len());
        } else {
            let end = block[arg_start..]
                .find('\\')
                .map_or(b.len(), |p| arg_start + p);
            arg = &block[arg_start..end];
            i = end;
        }
        out.push(Tag {
            name: name.to_owned(),
            arg: arg.trim().to_owned(),
            nested,
        });
        if name == "t" && !nested {
            if let Some(p) = arg.find('\\') {
                scan_into(&arg[p..], true, out);
            }
        }
    }
}

/// Length in bytes of the tag name at the start of `b` (just past a backslash).
fn name_len(b: &[u8]) -> usize {
    match b.first() {
        None => 0,
        Some(b'1'..=b'4') => {
            if matches!(b.get(1), Some(b'c' | b'a')) {
                2
            } else {
                1
            }
        }
        Some(c) if c.is_ascii_alphabetic() => {
            let run = b.iter().take_while(|c| c.is_ascii_alphabetic()).count();
            // Longest known name that prefixes the letter run; an unknown run
            // is taken whole so it can be reported by name.
            KNOWN
                .iter()
                .filter(|k| b[..run].starts_with(k.as_bytes()))
                .map(|k| k.len())
                .max()
                .unwrap_or(run)
        }
        Some(_) => 0,
    }
}

/// The tags that make an event animated for the on-screen budget.
pub(crate) fn is_animation(name: &str) -> bool {
    matches!(
        name,
        "t" | "move" | "fad" | "fade" | "k" | "kf" | "K" | "ko"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn scale_sizes_scales_only_size_tags_inside_blocks() {
        assert_eq!(
            scale_sizes(
                r"{\fs130\bord5\xbord2\shad1.5\blur3\fscx130\fsp2\b1}fs10 \bord4",
                1.5
            ),
            r"{\fs195\bord7.5\xbord3\shad2.25\blur4.5\fscx130\fsp2\b1}fs10 \bord4"
        );
        assert_eq!(
            scale_sizes(r"{\t(0,120,\bord6\fscx100)}Hi{\bord2}", 0.75),
            r"{\t(0,120,\bord4.5\fscx100)}Hi{\bord1.5}"
        );
        assert_eq!(scale_sizes("plain", 2.0), "plain");
    }

    fn names(block: &str) -> Vec<String> {
        scan(block).into_iter().map(|t| t.name).collect()
    }

    #[test]
    fn splits_longest_known_name() {
        assert_eq!(
            names(r"\bord2\blur3\be1\b1\fscx90\fsp4\fnBangers\alpha&H40&\an8"),
            ["bord", "blur", "be", "b", "fscx", "fsp", "fn", "alpha", "an"]
        );
        let t = scan(r"\fnPatrick Hand\fs50");
        assert_eq!(t[0].arg, "Patrick Hand");
        assert_eq!(t[1].arg, "50");
    }

    #[test]
    fn colour_and_alpha_numbered_tags() {
        assert_eq!(names(r"\1c&H112233&\3a&H80&\c&HFFFFFF&"), ["1c", "3a", "c"]);
    }

    #[test]
    fn nested_tags_follow_their_t() {
        let tags = scan(r"\t(0,120,\fscx100\fscy100)\fad(0,200)");
        let got: Vec<_> = tags.iter().map(|t| (t.name.as_str(), t.nested)).collect();
        assert_eq!(
            got,
            [("t", false), ("fscx", true), ("fscy", true), ("fad", false)]
        );
    }

    #[test]
    fn unknown_names_are_kept_whole() {
        assert_eq!(names(r"\foo12\kf25"), ["foo", "kf"]);
    }

    #[test]
    fn unclosed_brace_is_text() {
        let p = pieces("a {\\i1}b {c");
        assert!(matches!(p[1], Piece::Block(b) if b == "\\i1"));
        assert!(matches!(p[2], Piece::Text(t) if t == "b {c"));
    }

    #[test]
    fn clip_inside_t_balances_parens() {
        let tags = scan(r"\t(0,100,\clip(0,0,10,10))\pos(1,2)");
        let got: Vec<_> = tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(got, ["t", "clip", "pos"]);
    }
}
