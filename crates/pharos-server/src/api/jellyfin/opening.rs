//! `GET /Items/{itemId}/Opening` — a book's first readable prose as plain
//! text, without the whole file (V1448).
//!
//! Many books carry no stored description, and the reader wants the
//! foreword or first chapter before deciding to download. The source file is
//! the one `/Download` would send ([`served_path`]), so a converted Kindle
//! book previews its EPUB. An EPUB is read through its central directory and
//! only the spine items needed to fill [`MAX_CHARS`] are inflated; text and
//! HTML files contribute only their first [`MAX_TEXT_BYTES`].
//!
//! Anything else (PDF, comics, an unconverted Kindle file) or a book with no
//! prose answers 404, counted on `pharos_book_opening_total{outcome}`.

use std::cell::RefCell;
use std::io::Read;
use std::path::Path;

use actix_web::{error, http::header, web, HttpResponse};
use pharos_core::MediaStore;
use quire::{Archive, ArchiveError, Content, El, Namespace, Publication};

use super::auth_extractor::MediaUser;
use super::download::served_path;
use super::media_source;
use crate::state::AppState;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/items/{id}/opening", web::get().to(opening));
}

/// The longest opening sent, in characters, the ellipsis included.
pub(crate) const MAX_CHARS: usize = 6000;

/// A spine item with less prose than this is a cover, title page or bare
/// contents list, not the opening.
const MIN_SECTION_CHARS: usize = 80;

/// Spine items inspected before giving up, so a book of hundreds of empty
/// pages cannot keep a worker busy.
const MAX_SECTIONS_SCANNED: usize = 40;

/// How much of a `.txt`, `.md` or `.html` file is read. Markup can precede
/// the prose by a long way, so this is well above [`MAX_CHARS`].
const MAX_TEXT_BYTES: u64 = 256 * 1024;

/// Element nesting walked; deeper content is dropped rather than risk the
/// worker's stack on a hostile book.
const MAX_DEPTH: usize = 256;

/// What a request for an opening came to. A dashboard contract: bounded
/// cardinality, stable strings from [`Outcome::label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Served,
    /// Readable file, but nothing past cover, title and contents pages.
    NoProse,
    /// A format with no text to show (PDF, comics, an unconverted Kindle file).
    Unsupported,
    /// The file could not be opened or parsed.
    Unreadable,
}

impl Outcome {
    #[cfg(test)]
    pub(crate) const ALL: [Outcome; 4] = [
        Outcome::Served,
        Outcome::NoProse,
        Outcome::Unsupported,
        Outcome::Unreadable,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Outcome::Served => "served",
            Outcome::NoProse => "no_prose",
            Outcome::Unsupported => "unsupported",
            Outcome::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Epub,
    Text,
    Html,
}

fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "epub" => Some(Kind::Epub),
        "txt" | "md" | "markdown" => Some(Kind::Text),
        "html" | "htm" | "xhtml" => Some(Kind::Html),
        _ => None,
    }
}

async fn opening(
    state: web::Data<AppState>,
    user: MediaUser,
    req: actix_web::HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_str = path.into_inner();
    let id = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorNotFound("unknown item"))?;
    super::require_item_access(&state, &user.0, id, "opening").await?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("unknown item"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let requested = media_source::requested_id(req.query_string());
    let item = media_source::select(media_source::Route::Download, item, requested.as_deref());

    let source = served_path(&state, &item).await;
    let Some(kind) = kind_of(&source) else {
        record(
            id,
            Outcome::Unsupported,
            0,
            &format!(
                "no text preview for extension {:?}",
                source.extension().unwrap_or_default()
            ),
        );
        return Err(error::ErrorNotFound("no opening for this format"));
    };

    let read = {
        let source = source.clone();
        web::block(move || read_opening(&source, kind)).await
    };
    let text = match read {
        Ok(Ok(text)) => text,
        Ok(Err(reason)) => {
            record(id, Outcome::Unreadable, 0, &reason);
            return Err(error::ErrorNotFound("source unreadable"));
        }
        Err(e) => {
            record(id, Outcome::Unreadable, 0, &format!("worker failed: {e}"));
            return Err(error::ErrorInternalServerError("opening worker failed"));
        }
    };
    if text.is_empty() {
        record(
            id,
            Outcome::NoProse,
            0,
            "no section reached the minimum prose length",
        );
        return Err(error::ErrorNotFound("no opening found"));
    }
    let chars = text.chars().count();
    record(id, Outcome::Served, chars, "");
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
        .body(text))
}

/// Counts and logs one verdict, carrying the reason's offending value.
fn record(id: pharos_core::MediaId, outcome: Outcome, chars: usize, reason: &str) {
    metrics::counter!("pharos_book_opening_total", "outcome" => outcome.label()).increment(1);
    if outcome == Outcome::Unreadable {
        tracing::warn!(
            media.id = id,
            outcome = outcome.label(),
            chars,
            reason,
            "opening"
        );
    } else {
        tracing::debug!(
            media.id = id,
            outcome = outcome.label(),
            chars,
            reason,
            "opening"
        );
    }
}

/// The bounded opening of the file at `path`; empty when it holds no prose.
/// The error is the cause, for the log.
fn read_opening(path: &Path, kind: Kind) -> Result<String, String> {
    match kind {
        Kind::Epub => {
            let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
            let zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip: {e}"))?;
            let book = Publication::open_epub_from(Box::new(FileArchive(RefCell::new(zip))))
                .map_err(|e| format!("not an epub: {e}"))?;
            Ok(epub_opening(&book))
        }
        Kind::Text | Kind::Html => {
            let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
            let mut bytes = Vec::new();
            file.take(MAX_TEXT_BYTES)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("read: {e}"))?;
            // A read that stops mid-character must not turn into U+FFFD.
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_owned(),
                Err(e) if e.error_len().is_none() => {
                    String::from_utf8_lossy(bytes.get(..e.valid_up_to()).unwrap_or_default())
                        .into_owned()
                }
                Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
            };
            Ok(match kind {
                Kind::Html => bound(&html_to_text(&text)),
                _ => bound(&plain_text(&text)),
            })
        }
    }
}

/// The zip behind an EPUB, read from disk entry by entry so a large book is
/// never held whole.
struct FileArchive(RefCell<zip::ZipArchive<std::fs::File>>);

impl Archive for FileArchive {
    fn read(&self, path: &str) -> Result<Vec<u8>, ArchiveError> {
        let mut zip = self.0.borrow_mut();
        let entry = zip.by_name(path).map_err(|e| match e {
            zip::result::ZipError::FileNotFound => ArchiveError::Missing {
                path: path.to_owned(),
            },
            source => ArchiveError::Entry {
                path: path.to_owned(),
                source,
            },
        })?;
        if entry.size() > quire::MAX_ENTRY_BYTES {
            return Err(ArchiveError::TooLarge {
                path: path.to_owned(),
            });
        }
        let mut out = Vec::new();
        // The declared size can lie; the cap holds for what actually inflates.
        entry
            .take(quire::MAX_ENTRY_BYTES.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|source| ArchiveError::Read {
                path: path.to_owned(),
                source,
            })?;
        if out.len() as u64 > quire::MAX_ENTRY_BYTES {
            return Err(ArchiveError::TooLarge {
                path: path.to_owned(),
            });
        }
        Ok(out)
    }

    fn size(&self, path: &str) -> Option<u64> {
        self.0.borrow_mut().by_name(path).ok().map(|e| e.size())
    }
}

/// Prose of the spine in reading order, sections too short to be prose
/// skipped, cut at [`MAX_CHARS`].
fn epub_opening(book: &Publication) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    for index in 0..book.spine().len().min(MAX_SECTIONS_SCANNED) {
        let section = match book.section(index) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(spine = index, error = %e, "opening: section skipped");
                continue;
            }
        };
        let mut c = Collector::default();
        walk(&section.root, &mut c, 0);
        let text = c.finish();
        let n = text.chars().count();
        if n < MIN_SECTION_CHARS {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
            chars = chars.saturating_add(2);
        }
        out.push_str(&text);
        chars = chars.saturating_add(n);
        if chars > MAX_CHARS {
            break;
        }
    }
    bound(&out)
}

/// `text` cut to [`MAX_CHARS`] at a word boundary, with an ellipsis when cut.
fn bound(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_CHARS {
        return text.to_owned();
    }
    let room = MAX_CHARS.saturating_sub(1);
    let mut cut: String = text.chars().take(room).collect();
    let on_boundary = text.chars().nth(room).is_some_and(char::is_whitespace);
    if !on_boundary {
        if let Some(i) = cut.rfind(char::is_whitespace) {
            if i > 0 {
                cut.truncate(i);
            }
        }
    }
    let mut cut = cut.trim_end().to_owned();
    cut.push('…');
    cut
}

/// Plain-text and Markdown source as prose: line endings unified, trailing
/// blanks dropped, runs of blank lines folded to one.
fn plain_text(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push('\n');
            blank = false;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Builds paragraphs: whitespace collapsed, blocks separated by a blank line.
#[derive(Default)]
struct Collector {
    out: String,
    cur: String,
    pending_space: bool,
    chars: usize,
}

impl Collector {
    fn full(&self) -> bool {
        self.chars > MAX_CHARS
    }

    fn text(&mut self, t: &str) {
        for ch in t.chars() {
            if self.full() {
                return;
            }
            if ch.is_whitespace() || ch == '\u{a0}' {
                self.pending_space = !self.cur.is_empty();
            } else {
                if self.pending_space {
                    self.cur.push(' ');
                    self.chars = self.chars.saturating_add(1);
                    self.pending_space = false;
                }
                self.cur.push(ch);
                self.chars = self.chars.saturating_add(1);
            }
        }
    }

    fn soft_break(&mut self) {
        self.pending_space = !self.cur.is_empty();
    }

    fn block(&mut self) {
        self.pending_space = false;
        if self.cur.is_empty() {
            return;
        }
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
            self.chars = self.chars.saturating_add(2);
        }
        self.out.push_str(&self.cur);
        self.cur.clear();
    }

    fn finish(mut self) -> String {
        self.block();
        self.out
    }
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "ul"
            | "ol"
            | "blockquote"
            | "pre"
            | "section"
            | "article"
            | "aside"
            | "header"
            | "footer"
            | "tr"
            | "table"
            | "dl"
            | "dt"
            | "dd"
            | "figure"
            | "figcaption"
            | "hr"
            | "body"
    )
}

fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "head" | "script" | "style" | "title" | "nav" | "svg" | "img" | "image"
    )
}

fn walk(el: &El, c: &mut Collector, depth: usize) {
    if c.full() || depth > MAX_DEPTH || el.ns == Namespace::Svg || is_skipped(&el.name) {
        return;
    }
    let block = is_block(&el.name);
    if block {
        c.block();
    }
    if el.name == "br" {
        c.soft_break();
    }
    for child in &el.children {
        match child {
            Content::Text(t) => c.text(t),
            Content::Element(e) => walk(e, c, depth.saturating_add(1)),
        }
    }
    if block {
        c.block();
    }
}

/// An HTML document as prose. Not a parser: tags are dropped, `script` and
/// `style` bodies skipped, block tags end a paragraph, common entities
/// decoded.
fn html_to_text(html: &str) -> String {
    let mut c = Collector::default();
    let mut rest = html;
    while !rest.is_empty() && !c.full() {
        let Some(lt) = rest.find('<') else {
            c.text(&decode_entities(rest));
            break;
        };
        c.text(&decode_entities(rest.get(..lt).unwrap_or_default()));
        let after = rest.get(lt..).unwrap_or_default();
        if let Some(comment) = after.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |i| {
                comment.get(i.saturating_add(3)..).unwrap_or_default()
            });
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let tag = after.get(1..gt).unwrap_or_default();
        rest = after.get(gt.saturating_add(1)..).unwrap_or_default();
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');
        if !closing && matches!(name.as_str(), "script" | "style" | "head") {
            let close = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            rest = match lower.find(&close) {
                Some(i) => rest.get(i..).unwrap_or_default(),
                None => "",
            };
        } else if is_block(&name) {
            c.block();
        } else if name == "br" {
            c.soft_break();
        }
    }
    c.finish()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or_default());
        let tail = rest.get(i..).unwrap_or_default();
        let decoded = tail
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| Some((end, entity(tail.get(1..end)?)?)));
        match decoded {
            Some((end, ch)) => {
                out.push(ch);
                rest = tail.get(end.saturating_add(1)..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "hellip" => Some('…'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn outcome_labels_are_distinct_and_stable() {
        let labels: std::collections::BTreeSet<_> =
            Outcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), Outcome::ALL.len(), "labels collide");
        assert_eq!(
            labels.into_iter().collect::<Vec<_>>(),
            ["no_prose", "served", "unreadable", "unsupported"]
        );
    }

    #[::core::prelude::v1::test]
    fn the_cut_lands_on_a_word_and_stays_within_the_bound() {
        let text = "word ".repeat(5000);
        let out = bound(&text);
        assert!(out.chars().count() <= MAX_CHARS);
        assert!(out.ends_with("word…"), "{:?}", &out[out.len() - 12..]);
        assert_eq!(bound("short  "), "short");
    }

    #[::core::prelude::v1::test]
    fn html_is_stripped_and_entities_decoded() {
        let html = "<html><head><title>T</title><style>p{}</style></head>\
            <body><h1>Head &amp; tail</h1><p>One\n  two&nbsp;three<br>four &#8212; &#x41;</p>\
            <script>var x = '<p>no</p>';</script><!-- c --><p>Next &bogus; &</p></body></html>";
        assert_eq!(
            html_to_text(html),
            "Head & tail\n\nOne two three four — A\n\nNext &bogus; &"
        );
    }

    #[::core::prelude::v1::test]
    fn plain_text_folds_blank_runs() {
        assert_eq!(plain_text("\r\n\r\nA  \r\n\r\n\r\n\r\nB\nC\n"), "A\n\nB\nC");
    }

    #[::core::prelude::v1::test]
    fn extensions_choose_the_reader() {
        assert_eq!(kind_of(Path::new("/x/a.EPUB")), Some(Kind::Epub));
        assert_eq!(kind_of(Path::new("/x/a.md")), Some(Kind::Text));
        assert_eq!(kind_of(Path::new("/x/a.htm")), Some(Kind::Html));
        assert_eq!(kind_of(Path::new("/x/a.pdf")), None);
        assert_eq!(kind_of(Path::new("/x/a.azw3")), None);
    }
}
