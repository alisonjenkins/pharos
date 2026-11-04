//! One reading-order item, sanitised: the tree a renderer builds from, the
//! stylesheets that apply to it, and the text positions count in.

use crate::css::{self, Css};
use crate::path::{self, Href};
use crate::xml::{self, Element, Node};

/// Which namespace a renderer must create an element in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Html,
    Svg,
}

impl Namespace {
    pub fn uri(self) -> &'static str {
        match self {
            Namespace::Html => "http://www.w3.org/1999/xhtml",
            Namespace::Svg => "http://www.w3.org/2000/svg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Element(El),
    Text(String),
}

/// A sanitised element. Only allow-listed names and attributes survive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct El {
    pub name: String,
    pub ns: Namespace,
    pub attrs: Vec<Attr>,
    pub children: Vec<Content>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    /// A file inside the book by archive path; the renderer supplies its URL.
    Resource(String),
    /// A `style` attribute.
    Style(Css),
    /// A link to a place in the book. Rendered as `data-quire-link`; a
    /// renderer turns a click on it into a jump, never a navigation.
    Link(Target),
}

/// A place in the book: a reading-order index and an optional element id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub spine: usize,
    pub fragment: Option<String>,
}

impl Target {
    /// `"<spine>"` or `"<spine>#<fragment>"`, the `data-quire-link` value.
    pub fn encode(&self) -> String {
        match &self.fragment {
            Some(f) => format!("{}#{f}", self.spine),
            None => self.spine.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub index: usize,
    /// Archive path of the source document.
    pub path: String,
    /// The body, as a `div` carrying the body's id, class and language.
    pub root: El,
    /// Stylesheets in document order: linked ones, then `<style>` elements.
    pub styles: Vec<Css>,
}

impl Section {
    /// All text in document order. Positions are offsets into this string,
    /// counted in UTF-16 code units (see [`crate::Locator`]).
    pub fn text(&self) -> String {
        let mut out = String::new();
        walk_text(&self.root, &mut |t| out.push_str(t));
        out
    }

    pub fn text_len_utf16(&self) -> usize {
        let mut n = 0usize;
        walk_text(&self.root, &mut |t| {
            n = n.saturating_add(t.encode_utf16().count())
        });
        n
    }

    /// Archive paths of every image and stylesheet resource used, once each.
    pub fn resources(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut add = |p: &str| {
            if seen.insert(p.to_owned()) {
                out.push(p.to_owned());
            }
        };
        for css in &self.styles {
            css.resources().for_each(&mut add);
        }
        walk_attrs(&self.root, &mut |v| match v {
            Value::Resource(p) => add(p),
            Value::Style(css) => css.resources().for_each(&mut add),
            Value::Text(_) | Value::Link(_) => {}
        });
        out
    }

    /// Serialises the body as HTML. Meant for tests and simple consumers; a
    /// renderer that needs exact text offsets builds DOM nodes from
    /// [`Section::root`] instead, because the HTML parser rewrites some
    /// whitespace (a leading newline in `pre`, for one).
    pub fn to_html(&self, resolve: &mut dyn FnMut(&str) -> Option<String>) -> String {
        let mut out = String::new();
        write_html(&self.root, resolve, &mut out);
        out
    }
}

fn walk_text(el: &El, f: &mut dyn FnMut(&str)) {
    for c in &el.children {
        match c {
            Content::Text(t) => f(t),
            Content::Element(e) => walk_text(e, f),
        }
    }
}

fn walk_attrs(el: &El, f: &mut dyn FnMut(&Value)) {
    el.attrs.iter().for_each(|a| f(&a.value));
    for c in &el.children {
        if let Content::Element(e) = c {
            walk_attrs(e, f);
        }
    }
}

fn write_html(el: &El, resolve: &mut dyn FnMut(&str) -> Option<String>, out: &mut String) {
    out.push('<');
    out.push_str(&el.name);
    for a in &el.attrs {
        let value = match &a.value {
            Value::Text(t) => Some(t.clone()),
            Value::Resource(p) => resolve(p),
            Value::Style(css) => Some(css.render(resolve)),
            Value::Link(t) => Some(t.encode()),
        };
        if let Some(v) = value {
            out.push(' ');
            out.push_str(&a.name);
            out.push_str("=\"");
            out.push_str(&escape(&v, true));
            out.push('"');
        }
    }
    out.push('>');
    if VOID.contains(&el.name.as_str()) && el.ns == Namespace::Html {
        return;
    }
    for c in &el.children {
        match c {
            Content::Text(t) => out.push_str(&escape(t, false)),
            Content::Element(e) => write_html(e, resolve, out),
        }
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

const VOID: &[&str] = &["br", "col", "hr", "img", "wbr"];

/// HTML elements kept, with their children.
const HTML_KEEP: &[&str] = &[
    "a",
    "abbr",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "u",
    "ul",
    "var",
    "wbr",
];

/// SVG elements kept, by their case-sensitive names.
const SVG_KEEP: &[&str] = &[
    "svg",
    "g",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "image",
    "title",
    "desc",
    "defs",
    "linearGradient",
    "radialGradient",
    "stop",
];

/// Dropped together with everything inside them. Anything else not kept is
/// unwrapped: the element goes, its children stay.
const DROP: &[&str] = &[
    "script",
    "style",
    "iframe",
    "frame",
    "frameset",
    "object",
    "embed",
    "applet",
    "form",
    "input",
    "button",
    "select",
    "textarea",
    "option",
    "noscript",
    "template",
    "link",
    "meta",
    "base",
    "audio",
    "video",
    "source",
    "track",
    "canvas",
    "head",
    "foreignobject",
    "use",
    "animate",
    "animatemotion",
    "animatetransform",
    "set",
    "dialog",
    "portal",
    "picture",
];

const GLOBAL_ATTRS: &[&str] = &["id", "class", "lang", "dir", "title", "role"];

fn html_attrs(el: &str) -> &'static [&'static str] {
    match el {
        "img" => &["alt", "width", "height"],
        "td" | "th" => &["colspan", "rowspan", "headers", "scope"],
        "col" | "colgroup" => &["span"],
        "ol" => &["start", "reversed", "type"],
        "li" => &["value"],
        "time" => &["datetime"],
        "data" => &["value"],
        "details" => &["open"],
        _ => &[],
    }
}

const SVG_ATTRS: &[&str] = &[
    "viewBox",
    "width",
    "height",
    "x",
    "y",
    "x1",
    "x2",
    "y1",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "d",
    "points",
    "fill",
    "stroke",
    "stroke-width",
    "transform",
    "preserveAspectRatio",
    "opacity",
    "fill-opacity",
    "stroke-opacity",
    "offset",
    "stop-color",
    "stop-opacity",
    "gradientUnits",
    "gradientTransform",
    "font-size",
    "font-family",
    "text-anchor",
    "version",
];

/// What the sanitiser needs to know about the rest of the book.
pub(crate) struct Book<'a> {
    pub path: &'a str,
    pub index: usize,
    /// Reading-order index of a document, by archive path.
    pub spine_of: &'a dyn Fn(&str) -> Option<usize>,
    /// The text of a linked stylesheet, by archive path.
    pub stylesheet: &'a dyn Fn(&str) -> Option<String>,
}

pub(crate) fn sanitise(doc: &Element, book: &Book<'_>) -> Section {
    let mut styles = Vec::new();
    if let Some(head) = doc.elements().find(|e| e.is("head")) {
        for e in head.descendants() {
            if e.is("link")
                && e.attr("rel").is_some_and(|r| {
                    r.to_ascii_lowercase()
                        .split_whitespace()
                        .any(|t| t == "stylesheet")
                })
            {
                if let Some(Href::Internal { path, .. }) =
                    e.attr("href").map(|h| path::resolve(book.path, h))
                {
                    if let Some(text) = (book.stylesheet)(&path) {
                        styles.push(css::sanitise(&text, &path));
                    }
                }
            } else if e.is("style") {
                styles.push(css::sanitise(&e.text(), book.path));
            }
        }
    }
    let body = doc.elements().find(|e| e.is("body"));
    let mut root = El {
        name: "div".into(),
        ns: Namespace::Html,
        attrs: vec![Attr {
            name: "data-quire-body".into(),
            value: Value::Text(String::new()),
        }],
        children: Vec::new(),
    };
    let lang = doc.attr("lang").map(str::to_owned);
    if let Some(body) = body {
        root.attrs.extend(attrs(body, "div", Namespace::Html, book));
        root.children = children(body, Namespace::Html, book);
        // Body-level `<style>` is invalid but common.
        body_styles(body, book.path, &mut styles);
    }
    if let Some(lang) = lang {
        if !root.attrs.iter().any(|a| a.name == "lang") {
            root.attrs.push(Attr {
                name: "lang".into(),
                value: Value::Text(lang),
            });
        }
    }
    Section {
        index: book.index,
        path: book.path.to_owned(),
        root,
        styles,
    }
}

/// `<style>` elements in the body, skipping any inside content the
/// sanitiser drops (`template`, `noscript`, `foreignObject`, ...).
fn body_styles(el: &Element, base: &str, out: &mut Vec<Css>) {
    for e in el.elements() {
        if e.is("style") {
            out.push(css::sanitise(&e.text(), base));
        } else if !DROP.contains(&e.local_lower().as_str()) {
            body_styles(e, base, out);
        }
    }
}

fn children(el: &Element, ns: Namespace, book: &Book<'_>) -> Vec<Content> {
    let mut out = Vec::new();
    for n in &el.children {
        match n {
            Node::Text(t) => push_text(&mut out, t),
            Node::Element(e) => match classify(e, ns) {
                Verdict::Keep(name, child_ns) => out.push(Content::Element(El {
                    attrs: attrs(e, &name, child_ns, book),
                    children: children(e, child_ns, book),
                    name,
                    ns: child_ns,
                })),
                Verdict::Unwrap => {
                    for c in children(e, ns, book) {
                        match c {
                            Content::Text(t) => push_text(&mut out, &t),
                            el => out.push(el),
                        }
                    }
                }
                Verdict::Drop => {}
            },
        }
    }
    out
}

fn push_text(out: &mut Vec<Content>, t: &str) {
    if let Some(Content::Text(prev)) = out.last_mut() {
        prev.push_str(t);
    } else {
        out.push(Content::Text(t.to_owned()));
    }
}

enum Verdict {
    Keep(String, Namespace),
    Unwrap,
    Drop,
}

fn classify(e: &Element, ns: Namespace) -> Verdict {
    let local = xml::local_name(&e.name);
    let lower = local.to_ascii_lowercase();
    if DROP.contains(&lower.as_str()) {
        return Verdict::Drop;
    }
    if lower == "svg" {
        return Verdict::Keep("svg".into(), Namespace::Svg);
    }
    match ns {
        Namespace::Svg => SVG_KEEP
            .iter()
            .find(|k| k.eq_ignore_ascii_case(local))
            .map_or(Verdict::Unwrap, |k| {
                Verdict::Keep((*k).to_owned(), Namespace::Svg)
            }),
        Namespace::Html if HTML_KEEP.contains(&lower.as_str()) => {
            Verdict::Keep(lower, Namespace::Html)
        }
        Namespace::Html => Verdict::Unwrap,
    }
}

fn attrs(e: &Element, name: &str, ns: Namespace, book: &Book<'_>) -> Vec<Attr> {
    let mut out = Vec::new();
    for (key, value) in &e.attrs {
        let local = xml::local_name(key);
        let lower = local.to_ascii_lowercase();
        let text = |n: &str| Attr {
            name: n.to_owned(),
            value: Value::Text(value.clone()),
        };
        if lower == "style" {
            out.push(Attr {
                name: "style".into(),
                value: Value::Style(css::sanitise(value, book.path)),
            });
        } else if lower == "type" && key.contains(':') {
            // epub:type, kept for styling hooks without its namespace.
            out.push(text("data-epub-type"));
        } else if lower == "lang" {
            // xml:lang and lang both become lang.
            if !out.iter().any(|a: &Attr| a.name == "lang") {
                out.push(text("lang"));
            }
        } else if key.contains(':') && lower != "href" {
            // Other prefixed attributes (xmlns:*, ...) have no use here.
        } else if lower == "href" {
            out.extend(href(value, name, ns, book));
        } else if lower == "src" && name == "img" {
            if let Href::Internal { path, .. } = path::resolve(book.path, value) {
                out.push(Attr {
                    name: "src".into(),
                    value: Value::Resource(path),
                });
            }
        } else if lower.starts_with("aria-")
            || GLOBAL_ATTRS.contains(&lower.as_str())
            || (ns == Namespace::Html && html_attrs(name).contains(&lower.as_str()))
        {
            out.push(text(&lower));
        } else if let Some(svg) = SVG_ATTRS.iter().find(|a| a.eq_ignore_ascii_case(local)) {
            if ns == Namespace::Svg && svg_value_is_local(value) {
                out.push(text(svg));
            }
        }
    }
    out
}

/// A presentation attribute may hold `url(#id)`, but never a URL that loads.
/// A backslash could spell `url(` as a CSS escape, so any value with one is
/// refused outright.
fn svg_value_is_local(v: &str) -> bool {
    if v.contains('\\') {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    lower.match_indices("url(").all(|(i, m)| {
        lower.get(i.saturating_add(m.len())..).is_some_and(|rest| {
            rest.trim_start()
                .trim_start_matches(['"', '\''])
                .starts_with('#')
        })
    })
}

fn href(value: &str, name: &str, ns: Namespace, book: &Book<'_>) -> Vec<Attr> {
    let resolved = path::resolve(book.path, value);
    if ns == Namespace::Svg {
        // `<image href>`: a picture, never a link.
        return match (name, resolved) {
            ("image", Href::Internal { path, .. }) => vec![Attr {
                name: "href".into(),
                value: Value::Resource(path),
            }],
            _ => Vec::new(),
        };
    }
    if name != "a" {
        return Vec::new();
    }
    match resolved {
        Href::External(url) => vec![
            Attr {
                name: "href".into(),
                value: Value::Text(url),
            },
            Attr {
                name: "target".into(),
                value: Value::Text("_blank".into()),
            },
            Attr {
                name: "rel".into(),
                value: Value::Text("noopener noreferrer".into()),
            },
        ],
        Href::SameDocument(fragment) => vec![link(book.index, Some(fragment))],
        Href::Internal { path, fragment } => match (book.spine_of)(&path) {
            Some(spine) => vec![link(spine, fragment)],
            // Inside the book but not a reading-order document: inert.
            None => Vec::new(),
        },
        Href::Refused => Vec::new(),
    }
}

fn link(spine: usize, fragment: Option<String>) -> Attr {
    Attr {
        name: "data-quire-link".into(),
        value: Value::Link(Target { spine, fragment }),
    }
}
