//! The XML adapter: the only module that knows `quick-xml`. Everything else
//! reads the small owned tree defined here.

use quick_xml::escape::{resolve_html5_entity, resolve_predefined_entity};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Deeper nesting than this is refused rather than recursed into, so a hostile
/// document cannot exhaust the stack of anything that walks the tree.
pub const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    /// Qualified name as written (`dc:title`, `xlink:href`, `p`).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, thiserror::Error)]
pub enum XmlError {
    #[error("malformed XML at byte {position}: {source}")]
    Syntax {
        position: u64,
        #[source]
        source: quick_xml::Error,
    },
    #[error("XML nests deeper than {MAX_DEPTH} elements")]
    TooDeep,
    #[error("XML has no root element")]
    NoRoot,
    #[error("XML is not UTF-8")]
    NotUtf8,
}

impl Element {
    /// The name without its prefix, ASCII-lowercased.
    pub fn local_lower(&self) -> String {
        local_name(&self.name).to_ascii_lowercase()
    }

    pub fn is(&self, local: &str) -> bool {
        local_name(&self.name).eq_ignore_ascii_case(local)
    }

    /// An attribute by its local name, ignoring any prefix.
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| local_name(k).eq_ignore_ascii_case(local))
            .map(|(_, v)| v.as_str())
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// Every descendant element, depth first, in document order.
    pub fn descendants(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        let mut stack: Vec<&Element> = self.elements().collect();
        stack.reverse();
        while let Some(e) = stack.pop() {
            out.push(e);
            let before = stack.len();
            stack.extend(e.elements());
            if let Some(added) = stack.get_mut(before..) {
                added.reverse();
            }
        }
        out
    }

    pub fn find(&self, local: &str) -> Option<&Element> {
        self.descendants().into_iter().find(|e| e.is(local))
    }

    /// All text below this element, concatenated.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let mut stack: Vec<&Node> = self.children.iter().rev().collect();
        while let Some(n) = stack.pop() {
            match n {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => stack.extend(e.children.iter().rev()),
            }
        }
        out
    }
}

pub fn local_name(qname: &str) -> &str {
    qname.rsplit_once(':').map_or(qname, |(_, l)| l)
}

pub fn parse_bytes(bytes: &[u8]) -> Result<Element, XmlError> {
    parse(utf8(bytes)?)
}

/// Like [`parse_bytes`], for an XHTML content document.
pub fn parse_html_bytes(bytes: &[u8]) -> Result<Element, XmlError> {
    parse_with(utf8(bytes)?, Mode::Html)
}

fn utf8(bytes: &[u8]) -> Result<&str, XmlError> {
    std::str::from_utf8(strip_bom(bytes)).map_err(|_| XmlError::NotUtf8)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Xml,
    /// HTML void elements (`<br>`, `<img>`) close at once even when a sloppy
    /// book writes them without `/>`.
    Html,
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
}

/// Parses a document into its root element. Lenient where real books are
/// sloppy: HTML named entities resolve, an end tag closes the nearest open
/// element of its name (and is ignored when none is open), and comments,
/// processing instructions and the doctype are dropped.
pub fn parse(text: &str) -> Result<Element, XmlError> {
    parse_with(text, Mode::Xml)
}

fn parse_with(text: &str, mode: Mode) -> Result<Element, XmlError> {
    let mut reader = Reader::from_str(text);
    let cfg = reader.config_mut();
    cfg.expand_empty_elements = true;
    cfg.check_end_names = false;
    cfg.allow_unmatched_ends = true;
    cfg.allow_dangling_amp = true;

    // A synthetic document node; its first element child is the root.
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        let event = reader.read_event().map_err(|source| XmlError::Syntax {
            position: reader.error_position(),
            source,
        })?;
        match event {
            Event::Start(start) => {
                if stack.len() > MAX_DEPTH {
                    return Err(XmlError::TooDeep);
                }
                let el = element(&start);
                let void = mode == Mode::Html
                    && VOID
                        .iter()
                        .any(|v| local_name(&el.name).eq_ignore_ascii_case(v));
                stack.push(el);
                if void {
                    close(&mut stack);
                }
            }
            Event::End(end) => {
                let name = end.name();
                let name: &str = name.as_ref();
                // Only an open element of this name closes, with any left open
                // inside it; a stray end tag is ignored.
                if let Some(depth) = stack.iter().skip(1).rposition(|e| e.name == name) {
                    while stack.len() > depth.saturating_add(1) {
                        close(&mut stack);
                    }
                }
            }
            Event::Text(t) => push_text(&mut stack, &t.xml_content(XmlVersion::Implicit1_0)),
            Event::CData(c) => {
                let raw = c.into_inner();
                push_text(&mut stack, &raw);
            }
            Event::GeneralRef(r) => push_text(&mut stack, &entity(&r)),
            Event::Eof => break,
            Event::Empty(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_) => {}
        }
    }
    while stack.len() > 1 {
        close(&mut stack);
    }
    stack
        .pop()
        .and_then(|doc| {
            doc.children.into_iter().find_map(|n| match n {
                Node::Element(e) => Some(e),
                Node::Text(_) => None,
            })
        })
        .ok_or(XmlError::NoRoot)
}

fn element(start: &BytesStart<'_>) -> Element {
    let name = start.name().as_ref().to_owned();
    let attrs = start
        .attributes()
        .with_checks(false)
        .filter_map(Result::ok)
        .filter_map(|a| {
            let key = a.key.as_ref().to_owned();
            let value = a
                .normalized_value_with(XmlVersion::Implicit1_0, 1, resolve_any_entity)
                .ok()?
                .into_owned();
            Some((key, value))
        })
        .collect();
    Element {
        name,
        attrs,
        children: Vec::new(),
    }
}

fn resolve_any_entity(name: &str) -> Option<&'static str> {
    resolve_predefined_entity(name).or_else(|| resolve_html5_entity(name))
}

fn entity(r: &BytesRef<'_>) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    let name: &str = r.as_ref();
    resolve_any_entity(name).unwrap_or_default().to_owned()
}

fn close(stack: &mut Vec<Element>) {
    if stack.len() < 2 {
        return;
    }
    if let Some(done) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(Node::Element(done));
        }
    }
}

fn push_text(stack: &mut [Element], text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(top) = stack.last_mut() else { return };
    // An entity splits a run into several events; join them back into one
    // text node so offsets count the same text the DOM will hold.
    if let Some(Node::Text(prev)) = top.children.last_mut() {
        prev.push_str(text);
    } else {
        top.children.push(Node::Text(text.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn entities_join_the_text_around_them() {
        let root = parse("<p>Sense &amp; Sensibility&nbsp;&#8212;x</p>").unwrap();
        assert_eq!(
            root.children,
            vec![Node::Text("Sense & Sensibility\u{a0}\u{2014}x".into())]
        );
    }

    #[test]
    fn attributes_resolve_html_entities_and_keep_prefixes() {
        let root = parse(r#"<a xlink:href="a&amp;b" title="&hellip;"/>"#).unwrap();
        assert_eq!(root.attr("href"), Some("a&b"));
        assert_eq!(root.attr("title"), Some("\u{2026}"));
        assert_eq!(root.attrs[0].0, "xlink:href");
    }

    #[test]
    fn void_elements_close_themselves_in_html() {
        let root =
            parse_with("<div><p>a<img src='x'>b<br>c</p><p>d</p></div>", Mode::Html).unwrap();
        let ps: Vec<_> = root.elements().collect();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].children.len(), 5, "a, img, b, br, c are siblings");
        assert_eq!(ps[1].text(), "d");
    }

    #[test]
    fn a_stray_end_tag_closes_nothing() {
        let root = parse("<div><p>a</span>b</p>c</div>").unwrap();
        assert_eq!(root.elements().next().unwrap().text(), "ab");
        assert_eq!(root.text(), "abc");
    }

    #[test]
    fn an_unclosed_element_is_closed_at_the_end() {
        let root = parse("<div><p>one<p>two</div>").unwrap();
        assert_eq!(root.text(), "onetwo");
    }

    #[test]
    fn deep_nesting_is_refused() {
        let doc = "<a>".repeat(MAX_DEPTH + 2);
        assert!(matches!(parse(&doc), Err(XmlError::TooDeep)));
    }

    #[test]
    fn a_byte_order_mark_is_ignored() {
        let root = parse_bytes(b"\xEF\xBB\xBF<r>x</r>").unwrap();
        assert_eq!(root.name, "r");
    }
}
