//! Resolving the relative URLs a book's files use into archive entry paths.

/// What an `href` or `src` inside the book points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Href {
    /// A file inside the book, by archive path, with any `#fragment`.
    Internal {
        path: String,
        fragment: Option<String>,
    },
    /// A fragment of the document the reference is in.
    SameDocument(String),
    /// An absolute `http:` or `https:` URL.
    External(String),
    /// Anything else (`javascript:`, `data:`, `file:`, a path that climbs out
    /// of the book, an empty value). Never followed or fetched.
    Refused,
}

/// Resolves `href` as written in the file at archive path `base`.
pub fn resolve(base: &str, href: &str) -> Href {
    let href = href.trim();
    if href.is_empty() {
        return Href::Refused;
    }
    if let Some(scheme) = scheme(href) {
        return if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            Href::External(href.to_owned())
        } else {
            Href::Refused
        };
    }
    if href.starts_with("//") {
        return Href::Refused;
    }
    let (rest, fragment) = match href.split_once('#') {
        Some((r, f)) => (r, Some(percent_decode(f))),
        None => (href, None),
    };
    let rest = rest.split_once('?').map_or(rest, |(r, _)| r);
    if rest.is_empty() {
        return fragment.map_or(Href::Refused, Href::SameDocument);
    }
    let joined = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let dir = base.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{rest}")
    };
    match normalise(&percent_decode(&joined)) {
        Some(path) => Href::Internal { path, fragment },
        None => Href::Refused,
    }
}

/// `scheme` of `scheme:rest`, when the text before the first `:` is a valid
/// URL scheme (so `c:\x` on its own is not taken for a path).
fn scheme(href: &str) -> Option<&str> {
    let (head, _) = href.split_once(':')?;
    let mut chars = head.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(head)
}

/// Collapses `.` and `..` segments. `None` when `..` would climb above the
/// archive root.
fn normalise(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let decoded = (b == b'%')
            .then(|| bytes.get(i.saturating_add(1)..i.saturating_add(3)))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(v) = decoded {
            out.push(v);
            i = i.saturating_add(3);
        } else {
            out.push(b);
            i = i.saturating_add(1);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn internal(path: &str, fragment: Option<&str>) -> Href {
        Href::Internal {
            path: path.into(),
            fragment: fragment.map(Into::into),
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_files_directory() {
        assert_eq!(
            resolve("OEBPS/text/c1.xhtml", "../img/a%20b.png"),
            internal("OEBPS/img/a b.png", None)
        );
        assert_eq!(
            resolve("c1.xhtml", "c2.xhtml#s2"),
            internal("c2.xhtml", Some("s2"))
        );
        assert_eq!(
            resolve("OEBPS/c1.xhtml", "#n1"),
            Href::SameDocument("n1".into())
        );
    }

    #[test]
    fn nothing_outside_the_book_is_internal() {
        assert_eq!(resolve("a/b.xhtml", "../../etc/passwd"), Href::Refused);
        assert_eq!(resolve("a.xhtml", "javascript:alert(1)"), Href::Refused);
        assert_eq!(resolve("a.xhtml", " JavaScript:alert(1)"), Href::Refused);
        assert_eq!(resolve("a.xhtml", "data:text/html,x"), Href::Refused);
        assert_eq!(resolve("a.xhtml", "//evil.example/x.png"), Href::Refused);
        assert_eq!(resolve("a.xhtml", ""), Href::Refused);
        assert_eq!(
            resolve("a.xhtml", "https://example.org/"),
            Href::External("https://example.org/".into())
        );
    }
}
