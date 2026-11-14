//! OPDS 1.2 Atom documents (045): navigation and acquisition feeds, and the
//! OpenSearch description. Pure: a feed is built from plain values, so its
//! shape is tested without a server.
//!
//! Every text and attribute value passes through [`text`], which escapes the
//! five XML specials and drops characters XML 1.0 cannot carry, so a title
//! with a stray control byte still yields a document a reader parses.

use std::fmt::Write as _;

pub const NAVIGATION_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";
pub const ACQUISITION_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";
pub const OPENSEARCH_TYPE: &str = "application/opensearchdescription+xml";
pub const REL_ACQUISITION: &str = "http://opds-spec.org/acquisition";
pub const REL_IMAGE: &str = "http://opds-spec.org/image";
pub const REL_THUMBNAIL: &str = "http://opds-spec.org/image/thumbnail";

/// `s` made safe as XML character data or an attribute value.
pub fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            '\u{FFFE}' | '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Unix seconds as an Atom date (RFC 3339, UTC).
pub fn atom_date(unix_secs: i64) -> String {
    // The Jellyfin formatter adds a seven-digit fraction; Atom needs none.
    let full = pharos_jellyfin_api::dto::format_iso8601(unix_secs);
    let whole = full.split('.').next().unwrap_or_default();
    format!("{whole}Z")
}

/// A link in a feed's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub rel: &'static str,
    pub href: String,
    pub kind: &'static str,
    pub title: Option<String>,
}

impl Link {
    pub fn new(rel: &'static str, href: impl Into<String>, kind: &'static str) -> Self {
        Self {
            rel,
            href: href.into(),
            kind,
            title: None,
        }
    }
}

fn write_link(out: &mut String, link: &Link) {
    let _ = write!(
        out,
        "<link rel=\"{}\" href=\"{}\" type=\"{}\"",
        text(link.rel),
        text(&link.href),
        text(link.kind)
    );
    if let Some(title) = &link.title {
        let _ = write!(out, " title=\"{}\"", text(title));
    }
    out.push_str("/>");
}

/// What every feed carries in its head.
#[derive(Debug, Clone)]
pub struct Head {
    /// A stable `urn:` id.
    pub id: String,
    pub title: String,
    pub updated: i64,
    /// `self`, `start`, `up`, `search`, paging.
    pub links: Vec<Link>,
}

/// Paging facts, written as OpenSearch elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paging {
    pub total: usize,
    pub per_page: usize,
    /// Zero-based offset of the first entry.
    pub start: usize,
}

fn open_feed(out: &mut String, head: &Head, paging: Option<Paging>) {
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" \
         xmlns:dc=\"http://purl.org/dc/terms/\" \
         xmlns:opds=\"http://opds-spec.org/2010/catalog\" \
         xmlns:opensearch=\"http://a9.com/-/spec/opensearch/1.1/\">",
    );
    let _ = write!(
        out,
        "<id>{}</id><title>{}</title><updated>{}</updated>",
        text(&head.id),
        text(&head.title),
        atom_date(head.updated)
    );
    out.push_str("<author><name>Pharos</name></author>");
    if let Some(p) = paging {
        let _ = write!(
            out,
            "<opensearch:totalResults>{}</opensearch:totalResults>\
             <opensearch:itemsPerPage>{}</opensearch:itemsPerPage>\
             <opensearch:startIndex>{}</opensearch:startIndex>",
            p.total,
            p.per_page,
            p.start.saturating_add(1)
        );
    }
    for link in &head.links {
        write_link(out, link);
    }
}

/// One entry of a navigation feed: a link to another feed.
#[derive(Debug, Clone)]
pub struct NavEntry {
    pub id: String,
    pub title: String,
    pub summary: Option<String>,
    pub href: String,
    /// The linked feed's type: navigation or acquisition.
    pub kind: &'static str,
}

/// A navigation feed.
pub fn navigation(head: &Head, paging: Option<Paging>, entries: &[NavEntry]) -> String {
    let mut out = String::new();
    open_feed(&mut out, head, paging);
    for e in entries {
        let _ = write!(
            out,
            "<entry><title>{}</title><id>{}</id><updated>{}</updated>",
            text(&e.title),
            text(&e.id),
            atom_date(head.updated)
        );
        if let Some(summary) = &e.summary {
            let _ = write!(out, "<content type=\"text\">{}</content>", text(summary));
        }
        write_link(
            &mut out,
            &Link {
                rel: "subsection",
                href: e.href.clone(),
                kind: e.kind,
                title: None,
            },
        );
        out.push_str("</entry>");
    }
    out.push_str("</feed>\n");
    out
}

/// One book in an acquisition feed.
#[derive(Debug, Clone)]
pub struct BookEntry {
    pub id: String,
    pub title: String,
    pub author: Option<(String, String)>,
    pub updated: i64,
    pub summary: Option<String>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub isbn: Option<String>,
    /// Cover and thumbnail hrefs, when the book has a cover.
    pub images: Option<(String, String)>,
    /// The acquisition link: href, MIME type, a short label for the format.
    pub acquisition: (String, &'static str, &'static str),
}

/// An acquisition feed of `entries`.
pub fn acquisition(head: &Head, paging: Option<Paging>, entries: &[BookEntry]) -> String {
    let mut out = String::new();
    open_feed(&mut out, head, paging);
    for e in entries {
        let _ = write!(
            out,
            "<entry><title>{}</title><id>{}</id><updated>{}</updated>",
            text(&e.title),
            text(&e.id),
            atom_date(e.updated)
        );
        if let Some((name, href)) = &e.author {
            let _ = write!(
                out,
                "<author><name>{}</name><uri>{}</uri></author>",
                text(name),
                text(href)
            );
        }
        if let Some(lang) = &e.language {
            let _ = write!(out, "<dc:language>{}</dc:language>", text(lang));
        }
        if let Some(publisher) = &e.publisher {
            let _ = write!(out, "<dc:publisher>{}</dc:publisher>", text(publisher));
        }
        if let Some(isbn) = &e.isbn {
            let _ = write!(
                out,
                "<dc:identifier>urn:isbn:{}</dc:identifier>",
                text(isbn)
            );
        }
        if let Some(summary) = &e.summary {
            let _ = write!(out, "<summary type=\"text\">{}</summary>", text(summary));
        }
        if let Some((cover, thumb)) = &e.images {
            write_link(&mut out, &Link::new(REL_IMAGE, cover.clone(), "image/jpeg"));
            write_link(
                &mut out,
                &Link::new(REL_THUMBNAIL, thumb.clone(), "image/jpeg"),
            );
        }
        let (href, mime, label) = &e.acquisition;
        write_link(
            &mut out,
            &Link {
                rel: REL_ACQUISITION,
                href: href.clone(),
                kind: mime,
                title: Some((*label).to_owned()),
            },
        );
        out.push_str("</entry>");
    }
    out.push_str("</feed>\n");
    out
}

/// The OpenSearch description: one Atom template, `q={searchTerms}`.
pub fn opensearch(template: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\
         <ShortName>Pharos</ShortName>\
         <Description>Search the books in Pharos</Description>\
         <InputEncoding>UTF-8</InputEncoding><OutputEncoding>UTF-8</OutputEncoding>\
         <Url type=\"{}\" template=\"{}\"/>\
         </OpenSearchDescription>\n",
        text(ACQUISITION_TYPE),
        text(template)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_specials_and_drops_what_xml_cannot_carry() {
        assert_eq!(text("a<b>&\"c'"), "a&lt;b&gt;&amp;&quot;c&apos;");
        assert_eq!(text("bell\u{7}tab\t"), "belltab\t");
        assert_eq!(text("ünïcode"), "ünïcode");
    }

    #[test]
    fn dates_are_rfc3339_utc() {
        assert_eq!(atom_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(atom_date(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
