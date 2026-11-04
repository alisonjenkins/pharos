//! Book stylesheets, made safe to apply: no remote fetches, no `@import`, no
//! `@font-face`, and `url()`s that point inside the book kept as references
//! the caller resolves when it draws.

use crate::path::{self, Href};

/// A sanitised stylesheet or `style` attribute.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Css {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// `url()` of a file inside the book, by archive path.
    Resource(String),
}

impl Css {
    /// Archive paths this stylesheet refers to.
    pub fn resources(&self) -> impl Iterator<Item = &str> {
        self.pieces.iter().filter_map(|p| match p {
            Piece::Resource(r) => Some(r.as_str()),
            Piece::Text(_) => None,
        })
    }

    /// The stylesheet text, each resource replaced by the URL `resolve` gives
    /// it (a `blob:` URL in a browser), or by `none` when it gives none. `</`
    /// is written `<\/` (the same CSS), so the text cannot end a `<style>`
    /// element it is placed in.
    pub fn render(&self, resolve: &mut dyn FnMut(&str) -> Option<String>) -> String {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text(t) => out.push_str(&t.replace("</", "<\\/")),
                Piece::Resource(r) => match resolve(r) {
                    Some(url) => {
                        out.push_str("url(\"");
                        out.push_str(&url.replace(['"', '\\', '\n', '<'], ""));
                        out.push_str("\")");
                    }
                    None => out.push_str("none"),
                },
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.pieces
            .iter()
            .all(|p| matches!(p, Piece::Text(t) if t.trim().is_empty()))
    }
}

/// Sanitises `css` written in the file at archive path `base`.
pub fn sanitise(css: &str, base: &str) -> Css {
    let mut s = Scanner {
        chars: css.chars().collect(),
        at: 0,
        out: Css::default(),
        base,
    };
    s.run();
    s.out
}

struct Scanner<'a> {
    chars: Vec<char>,
    at: usize,
    out: Css,
    base: &'a str,
}

impl Scanner<'_> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at.saturating_add(ahead)).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.at = self.at.saturating_add(1);
        Some(c)
    }

    fn text(&mut self, t: &str) {
        if let Some(Piece::Text(prev)) = self.out.pieces.last_mut() {
            prev.push_str(t);
        } else {
            self.out.pieces.push(Piece::Text(t.to_owned()));
        }
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '/' if self.peek(1) == Some('*') => self.skip_comment(),
                '"' | '\'' => {
                    let raw = self.read_string();
                    let shown = if looks_remote(&raw.value) {
                        "\"\"".to_owned()
                    } else {
                        raw.source
                    };
                    self.text(&shown);
                }
                '@' => self.at_rule(),
                c if is_ident_start(c) || c == '\\' => self.ident(),
                _ => {
                    self.bump();
                    self.text(&c.to_string());
                }
            }
        }
    }

    fn skip_comment(&mut self) {
        self.at = self.at.saturating_add(2);
        while let Some(c) = self.bump() {
            if c == '*' && self.peek(0) == Some('/') {
                self.bump();
                return;
            }
        }
    }

    /// Reads a quoted string at the cursor: its decoded value and its source.
    fn read_string(&mut self) -> Lexed {
        let start = self.at;
        let quote = self.bump().unwrap_or('"');
        let mut value = String::new();
        while let Some(c) = self.bump() {
            match c {
                c if c == quote => break,
                '\\' => {
                    if let Some(d) = self.escape() {
                        value.push(d);
                    }
                }
                '\n' => break,
                c => value.push(c),
            }
        }
        Lexed {
            value,
            source: self.source_since(start),
        }
    }

    fn source_since(&self, start: usize) -> String {
        self.chars
            .get(start..self.at)
            .map(|s| s.iter().collect())
            .unwrap_or_default()
    }

    /// Decodes an escape whose backslash was just consumed.
    fn escape(&mut self) -> Option<char> {
        let mut hex = String::new();
        while hex.len() < 6 {
            match self.peek(0) {
                Some(h) if h.is_ascii_hexdigit() => {
                    hex.push(h);
                    self.bump();
                }
                _ => break,
            }
        }
        if hex.is_empty() {
            return self.bump();
        }
        if matches!(self.peek(0), Some(' ' | '\t' | '\n')) {
            self.bump();
        }
        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
    }

    /// An identifier, decoded, so `\75 rl(` is seen as the `url(` it is. An
    /// escape that decodes to anything but a name character keeps its
    /// original spelling, so decoding never adds syntax.
    fn read_ident(&mut self) -> Lexed {
        let start = self.at;
        let mut value = String::new();
        while let Some(c) = self.peek(0) {
            if c == '\\' {
                let esc_start = self.at;
                self.bump();
                match self.escape() {
                    Some(d) if is_name_char(d) => value.push(d),
                    _ => value.push_str(&self.source_since(esc_start)),
                }
            } else if is_name_char(c) {
                value.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Lexed {
            value,
            source: self.source_since(start),
        }
    }

    fn ident(&mut self) {
        let name = self.read_ident();
        if name.value.is_empty() {
            // A lone backslash that escaped nothing useful.
            let c = self.bump();
            self.text(&c.map(String::from).unwrap_or_default());
            return;
        }
        if self.peek(0) == Some('(') && name.value.eq_ignore_ascii_case("url") {
            self.bump();
            self.url();
        } else {
            self.text(&name.value);
        }
    }

    /// The argument of a `url(` whose parenthesis was just consumed.
    fn url(&mut self) {
        self.skip_space();
        let target = if matches!(self.peek(0), Some('"' | '\'')) {
            let s = self.read_string().value;
            self.skip_space();
            if self.peek(0) == Some(')') {
                self.bump();
            }
            s
        } else {
            let mut raw = String::new();
            while let Some(c) = self.bump() {
                match c {
                    ')' => break,
                    '\\' => raw.extend(self.escape()),
                    c => raw.push(c),
                }
            }
            raw.trim().to_owned()
        };
        match path::resolve(self.base, &target) {
            Href::Internal { path, .. } => self.out.pieces.push(Piece::Resource(path)),
            _ => self.text("none"),
        }
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(0), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    fn at_rule(&mut self) {
        self.bump();
        let name = self.read_ident();
        match name.value.to_ascii_lowercase().as_str() {
            "import" => self.skip_until_semicolon(),
            "font-face" => self.skip_block(),
            _ => {
                self.text("@");
                self.text(&name.value);
            }
        }
    }

    fn skip_until_semicolon(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '"' | '\'' => {
                    self.read_string();
                }
                ';' => {
                    self.bump();
                    return;
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn skip_block(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek(0) {
            match c {
                '"' | '\'' => {
                    self.read_string();
                }
                '{' => {
                    depth = depth.saturating_add(1);
                    self.bump();
                }
                '}' => {
                    self.bump();
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                ';' if depth == 0 => {
                    self.bump();
                    return;
                }
                _ => {
                    self.bump();
                }
            }
        }
    }
}

struct Lexed {
    value: String,
    source: String,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '-' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || !c.is_ascii()
}

/// A string that would fetch from outside the book if a property took it as
/// a URL (`image-set("https://…")`, `src("//…")`).
fn looks_remote(s: &str) -> bool {
    let s = s.trim();
    // URL parsing strips tabs and newlines anywhere and treats `\` as `/`,
    // so normalise the same way before looking. Only a URL with a fetching
    // scheme, or a scheme-relative one, loads; `content: "Note: x"` survives.
    let s: String = s
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .map(|c| {
            if c == '\\' {
                '/'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let s = s.trim_start_matches(|c: char| c <= ' ');
    s.starts_with("//")
        || ["http:", "https:", "ftp:", "ws:", "wss:", "file:"]
            .iter()
            .any(|scheme| s.starts_with(scheme))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn show(css: &str) -> String {
        sanitise(css, "OEBPS/css/book.css").render(&mut |p| Some(format!("blob:{p}")))
    }

    #[test]
    fn internal_urls_become_resources_and_remote_ones_none() {
        assert_eq!(
            show("p { background: url(../img/a.png) no-repeat }"),
            "p { background: url(\"blob:OEBPS/img/a.png\") no-repeat }"
        );
        assert_eq!(
            show("p{background:url('https://x.example/a.png')}"),
            "p{background:none}"
        );
        assert_eq!(
            show("p{background:url(//x.example/a.png)}"),
            "p{background:none}"
        );
    }

    #[test]
    fn escaped_url_functions_are_still_caught() {
        assert_eq!(
            show(r"p{background:\75 rl(https://x.example/a)}"),
            "p{background:none}"
        );
        assert_eq!(
            show(r"p{background:u\72l(https://x.example/a)}"),
            "p{background:none}"
        );
    }

    #[test]
    fn imports_and_font_faces_are_dropped() {
        assert_eq!(
            show("@import url(https://x.example/a.css);@font-face{font-family:X;src:url(f.otf)}p{color:red}"),
            "p{color:red}"
        );
        assert_eq!(show("@IMPORT 'https://x.example/a.css'; p{}"), " p{}");
    }

    #[test]
    fn remote_strings_are_emptied_and_others_kept() {
        assert_eq!(
            show(r#"p{background:image-set("https://x.example/a.png" 1x)}"#),
            r#"p{background:image-set("" 1x)}"#
        );
        assert_eq!(
            show(r#"q::before{content:"\201C"}"#),
            r#"q::before{content:"\201C"}"#
        );
        assert_eq!(
            show(r#"p::before{content:"Note: "}"#),
            r#"p::before{content:"Note: "}"#
        );
    }

    #[test]
    fn remote_strings_hiding_their_scheme_are_emptied() {
        for s in [
            r#""https:/evil.example/x.png""#,
            "\"https:\t//evil.example/x.png\"",
            r#""HTTPS:\\evil.example\x.png""#,
            r#""https:\
//evil.example/x.png""#,
        ] {
            let css = format!("p{{background:image-set({s} 1x)}}");
            assert_eq!(show(&css), r#"p{background:image-set("" 1x)}"#, "{s}");
        }
    }

    #[test]
    fn a_style_end_tag_cannot_escape() {
        assert_eq!(
            show("p{}</style><img src=x onerror=alert(1)>"),
            r"p{}<\/style><img src=x onerror=alert(1)>"
        );
    }

    #[test]
    fn comments_go() {
        assert_eq!(show("p{/* url(https://x) */color:red}"), "p{color:red}");
    }
}
