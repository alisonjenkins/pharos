//! epub reading (004-books, US1).
//!
//! An epub is a zip holding `META-INF/container.xml`, which names an OPF
//! ("package") document, which carries Dublin Core metadata plus a manifest of
//! the book's files. pharos reads exactly that far — the reading experience is
//! jellyfin-web's `bookPlayer` (epub.js), which downloads the whole file and
//! paginates it in the browser.
//!
//! Parsed with `quick-xml`, already a dependency for the Kodi NFO reader, using
//! the same streaming event-reader style: an OPF carries repeated `<dc:*>` and
//! `<meta>` elements in no guaranteed order and tolerates unknown tags, which a
//! rigid derive handles badly.

use std::io::Read;
use std::path::Path;

use pharos_core::{BookFormat, BookMeta};
use quick_xml::events::Event;

use super::{push_entity, record_classify, ClassifyReason, ClassifyVerdict};

/// Everything an epub's OPF told us.
///
/// Wider than [`BookMeta`] on purpose: `title`, `date` and `description` are
/// ORDINARY item fields, which the metadata provider (T066) folds into the
/// resolver alongside every other source. Keeping them here means the file is
/// parsed once rather than once per consumer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpubMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    /// `dc:date`, verbatim. Often a full ISO timestamp, sometimes just a year;
    /// normalising is the provider's job, not the parser's.
    pub date: Option<String>,
    pub isbn: Option<String>,
    pub series_name: Option<String>,
    pub series_index: Option<u32>,
    /// Zip entry path of the cover image, when the OPF declares one. The BYTES
    /// are read separately (T061) so metadata-only callers do not pay for them.
    pub cover_href: Option<String>,
}

impl EpubMetadata {
    /// The book-specific subset that is persisted on the item.
    pub fn to_book_meta(&self) -> BookMeta {
        BookMeta {
            format: BookFormat::Epub,
            // Deliberately None: an epub has no stable page count, because
            // pagination is a function of the reader's viewport. Any number
            // pharos invented would be wrong on every device.
            page_count: None,
            author: self.author.clone(),
            publisher: self.publisher.clone(),
            series_name: self.series_name.clone(),
            series_index: self.series_index,
            isbn: self.isbn.clone(),
        }
    }
}

/// Why an epub could not be read. Carries the offending value in every case —
/// never a bare class (the "expose the cause" discipline: a reason that does not
/// name the value is another round of guessing).
#[derive(Debug)]
pub enum EpubError {
    Open(String),
    NotAZip(String),
    NoContainer(String),
    NoRootfile(String),
    MissingOpf { href: String, detail: String },
    Xml { entry: String, detail: String },
}

impl std::fmt::Display for EpubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpubError::Open(e) => write!(f, "cannot open file: {e}"),
            EpubError::NotAZip(e) => write!(f, "not a readable zip: {e}"),
            EpubError::NoContainer(e) => {
                write!(f, "no META-INF/container.xml: {e}")
            }
            EpubError::NoRootfile(detail) => {
                write!(f, "container.xml declares no rootfile ({detail})")
            }
            EpubError::MissingOpf { href, detail } => {
                write!(f, "OPF entry {href:?} not in the archive: {detail}")
            }
            EpubError::Xml { entry, detail } => {
                write!(f, "malformed XML in {entry:?}: {detail}")
            }
        }
    }
}

/// Read an epub's metadata.
///
/// Errors are returned rather than swallowed so the CALLER decides — and
/// [`read_epub_or_empty`] is that decision for the scan path: V6 says a parse
/// failure must not drop the item, so a malformed epub still imports, titled
/// from its filename.
pub fn read_epub(path: &Path) -> Result<EpubMetadata, EpubError> {
    let file = std::fs::File::open(path).map_err(|e| EpubError::Open(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| EpubError::NotAZip(e.to_string()))?;

    // 1. container.xml → the OPF's path inside the archive. Its location is
    //    fixed by the epub spec; the OPF's is not.
    let container = read_entry(&mut zip, "META-INF/container.xml")
        .map_err(|e| EpubError::NoContainer(e.to_string()))?;
    let opf_href = rootfile_href(&container).ok_or_else(|| {
        EpubError::NoRootfile(format!("{} bytes of container XML", container.len()))
    })?;

    // 2. the OPF itself.
    let opf = read_entry(&mut zip, &opf_href).map_err(|e| EpubError::MissingOpf {
        href: opf_href.clone(),
        detail: e.to_string(),
    })?;
    let mut meta = parse_opf(&opf).map_err(|detail| EpubError::Xml {
        entry: opf_href.clone(),
        detail,
    })?;

    // 3. The cover href in the OPF is relative to the OPF's own directory, not
    //    to the archive root. Resolve it now so callers get a zip entry path
    //    they can actually open.
    if let Some(href) = meta.cover_href.take() {
        meta.cover_href = Some(resolve_relative(&opf_href, &href));
    }
    Ok(meta)
}

/// Read the cover image's BYTES out of an epub.
///
/// A second open of the archive, deliberately: metadata is read for every book
/// on every scan pass, and the cover only for a book the scan is actually
/// writing. Both are O(1) in file size — a zip's central directory is read
/// without inflating anything but the entries actually named — so SC-002 holds
/// either way.
///
/// Returns `Ok(None)` when the OPF declared no cover at all, which is a normal
/// outcome and not an error.
pub fn read_epub_cover(path: &Path) -> Result<Option<Vec<u8>>, EpubError> {
    let Some(href) = read_epub(path)?.cover_href else {
        return Ok(None);
    };
    let file = std::fs::File::open(path).map_err(|e| EpubError::Open(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| EpubError::NotAZip(e.to_string()))?;
    let idx = (0..zip.len())
        .find(|i| {
            zip.by_index_raw(*i)
                .map(|e| e.name().eq_ignore_ascii_case(&href))
                .unwrap_or(false)
        })
        // A manifest that names an entry the archive does not contain. Rare,
        // and worth naming the href rather than reporting "no cover" — the two
        // have different fixes.
        .ok_or_else(|| EpubError::MissingOpf {
            href: href.clone(),
            detail: "declared as the cover but absent from the archive".into(),
        })?;
    let mut entry = zip.by_index(idx).map_err(|e| EpubError::MissingOpf {
        href: href.clone(),
        detail: e.to_string(),
    })?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| EpubError::MissingOpf {
            href,
            detail: e.to_string(),
        })?;
    Ok(Some(bytes))
}

/// The scan path's decision: a parse failure yields empty metadata rather than
/// dropping the item (V6), and records WHY on the classify counter.
///
/// Records only the PARSE outcome. The cover verdict belongs to the extractor
/// (`super::read_book_cover`), which is the only step that knows whether bytes
/// actually came out — recording `cover_found` here on the strength of a
/// manifest entry would report a cover for a book whose grid tile 404s, which
/// is the B149 shape the counter exists to make visible.
pub fn read_epub_or_empty(path: &Path) -> BookMeta {
    match read_epub(path) {
        Ok(meta) => meta.to_book_meta(),
        Err(err) => {
            let (verdict, reason) = match &err {
                EpubError::Open(_) => (ClassifyVerdict::Unparseable, ClassifyReason::Unopenable),
                _ => (
                    ClassifyVerdict::Unparseable,
                    ClassifyReason::MalformedContainer,
                ),
            };
            record_classify(BookFormat::Epub, verdict, reason);
            // The counter says the CLASS; the log says which file and why. Both,
            // because a rate without an example is undiagnosable and an example
            // without a rate is anecdote.
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "epub metadata unreadable; importing with filename title only"
            );
            BookMeta {
                format: BookFormat::Epub,
                ..Default::default()
            }
        }
    }
}

/// Read one zip entry to a `String`, tolerating case differences in the entry
/// name (some writers emit `META-INF/Container.xml`).
fn read_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String, std::io::Error> {
    let idx = (0..zip.len()).find(|i| {
        zip.by_index_raw(*i)
            .map(|e| e.name().eq_ignore_ascii_case(name))
            .unwrap_or(false)
    });
    let Some(idx) = idx else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no entry named {name:?}"),
        ));
    };
    let mut entry = zip
        .by_index(idx)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut out = String::new();
    entry.read_to_string(&mut out)?;
    Ok(out)
}

/// `<rootfile full-path="…">` out of container.xml.
fn rootfile_href(xml: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e))
                if local_name(e.name().as_ref()).eq_ignore_ascii_case(b"rootfile") =>
            {
                if let Some(v) = attr(&e, b"full-path") {
                    return Some(v);
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Parse the OPF `<metadata>` block plus enough of `<manifest>` to resolve the
/// cover.
fn parse_opf(xml: &str) -> Result<EpubMetadata, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    // NOT `trim_text(true)` — see the note in `comic::parse_comic_info`. An
    // entity splits the literal run, so per-run trimming eats the spaces around
    // it and a description containing `&amp;` comes back with words fused.
    reader.config_mut().trim_text(false);

    let mut out = EpubMetadata::default();
    // `<meta name="cover" content="ID">` names a MANIFEST item id; the href
    // lives on that item, which may appear before or after the meta tag. So
    // collect both and join at the end.
    let mut cover_id: Option<String> = None;
    let mut manifest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // Which `<dc:*>` element's text we are inside, if any, and the literal run
    // accumulated so far. quick-xml 0.41 delivers entities as separate
    // `GeneralRef` events, so a single element's text arrives in pieces —
    // `&amp;` in a title would otherwise truncate it. Same handling as the NFO
    // reader.
    let mut current: Option<Vec<u8>> = None;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref()).to_ascii_lowercase();
                match name.as_slice() {
                    b"title" | b"creator" | b"publisher" | b"description" | b"date"
                    | b"identifier" => {
                        current = Some(name);
                        text.clear();
                    }
                    b"meta" => absorb_meta(&e, &mut out, &mut cover_id),
                    b"item" => absorb_manifest_item(&e, &mut manifest),
                    _ => current = None,
                }
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref()).to_ascii_lowercase();
                match name.as_slice() {
                    // EPUB 3 puts the series on a `<meta property="…">` with
                    // text content, but calibre's EPUB 2 form is an empty tag
                    // carrying name/content attributes. Both reach here.
                    b"meta" => absorb_meta(&e, &mut out, &mut cover_id),
                    b"item" => absorb_manifest_item(&e, &mut manifest),
                    _ => {}
                }
            }
            // A decode failure drops that run rather than the element
            // (V6 tolerance), same as the NFO reader.
            Ok(Event::Text(t)) if current.is_some() => {
                if let Ok(decoded) = t.decode() {
                    text.push_str(&decoded);
                }
            }
            // quick-xml 0.41 delivers entities separately, so `&amp;` in a
            // title reassembles here rather than truncating it.
            Ok(Event::GeneralRef(r)) if current.is_some() => push_entity(&r, &mut text),
            Ok(Event::End(_)) => {
                if let Some(field) = current.take() {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        assign_dc(&field, trimmed, &mut out);
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) => break,
            // A truncated or malformed OPF: report it with the parser's own
            // message so the log names the actual problem.
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }

    out.cover_href = cover_id
        .and_then(|id| manifest.get(&id).cloned())
        // No `<meta name="cover">`: fall back to a manifest item whose id or
        // href looks like a cover. Deliberately narrow — guessing "the first
        // image in the book" would pick a chapter illustration as often as a
        // cover, and a wrong cover is worse than none.
        .or_else(|| {
            manifest
                .iter()
                .find(|(id, href)| {
                    id.to_ascii_lowercase().contains("cover")
                        || href.to_ascii_lowercase().contains("cover")
                })
                .map(|(_, href)| href.clone())
        });
    Ok(out)
}

fn assign_dc(field: &[u8], text: String, out: &mut EpubMetadata) {
    // First-wins within a file: an OPF may repeat `dc:creator` for
    // illustrators and translators, and the first is the author by convention.
    let slot = match field {
        b"title" => &mut out.title,
        b"creator" => &mut out.author,
        b"publisher" => &mut out.publisher,
        b"description" => &mut out.description,
        b"date" => &mut out.date,
        b"identifier" => {
            // Only keep an identifier that actually looks like an ISBN — the
            // element is also used for UUIDs and calibre's internal ids, and
            // storing one of those as an ISBN would be a lie the UI displays.
            let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
            if (digits.len() == 10 || digits.len() == 13) && out.isbn.is_none() {
                out.isbn = Some(text);
            }
            return;
        }
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(text);
    }
}

fn absorb_meta(
    e: &quick_xml::events::BytesStart<'_>,
    out: &mut EpubMetadata,
    cover_id: &mut Option<String>,
) {
    let name = attr(e, b"name").unwrap_or_default().to_ascii_lowercase();
    let content = attr(e, b"content");
    match name.as_str() {
        "cover" => {
            if let Some(c) = content {
                *cover_id = Some(c);
            }
        }
        // First-wins in both cases: a repeated meta tag does not overwrite an
        // earlier value.
        "calibre:series" if out.series_name.is_none() => {
            out.series_name = content.filter(|c| !c.trim().is_empty());
        }
        "calibre:series_index" if out.series_index.is_none() => {
            // "3.0" is the common calibre spelling; take the integer part.
            out.series_index = content
                .and_then(|c| c.split('.').next().map(str::to_string))
                .and_then(|c| c.trim().parse::<u32>().ok());
        }
        _ => {}
    }
}

fn absorb_manifest_item(
    e: &quick_xml::events::BytesStart<'_>,
    manifest: &mut std::collections::HashMap<String, String>,
) {
    // Only image items can be a cover; recording the whole manifest would make
    // the "looks like a cover" fallback match an XHTML wrapper page.
    let media_type = attr(e, b"media-type").unwrap_or_default();
    if !media_type.starts_with("image/") {
        return;
    }
    if let (Some(id), Some(href)) = (attr(e, b"id"), attr(e, b"href")) {
        manifest.insert(id, href);
    }
}

/// An attribute's value, matched on the LOCAL name so a namespace prefix does
/// not hide it.
fn attr(e: &quick_xml::events::BytesStart<'_>, want: &[u8]) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.local_name().as_ref().eq_ignore_ascii_case(want) {
            // `normalized_value` decodes, resolves predefined entities and
            // normalises per the XML spec — `unescape_value` is deprecated in
            // quick-xml 0.41. Same call the NFO reader makes.
            return a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|c| c.trim().to_string())
                .filter(|s| !s.is_empty());
        }
    }
    None
}

/// Strip a namespace prefix: `dc:creator` → `creator`.
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|b| *b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Resolve `href` relative to the directory holding `base`.
///
/// An OPF at `OEBPS/content.opf` naming `images/cover.jpg` means
/// `OEBPS/images/cover.jpg`. Getting this wrong is a silent no-cover, because
/// the zip lookup simply misses.
fn resolve_relative(base: &str, href: &str) -> String {
    let dir = match base.rfind('/') {
        Some(i) => &base[..i + 1],
        None => "",
    };
    let joined = format!("{dir}{href}");
    // Collapse any `..` the href used to climb out of the OPF's directory.
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal but SPEC-SHAPED epub: a zip whose first entry is an
    /// uncompressed `mimetype`, then `META-INF/container.xml`, then the OPF.
    /// Generated rather than checked in, so the repo carries no binary blob and
    /// the fixture's contents are visible in the test that uses them.
    fn write_epub(path: &Path, container: Option<&str>, opf: Option<(&str, &str)>) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("mimetype", stored).unwrap();
        zw.write_all(b"application/epub+zip").unwrap();
        if let Some(c) = container {
            zw.start_file(
                "META-INF/container.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(c.as_bytes()).unwrap();
        }
        if let Some((name, body)) = opf {
            zw.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Dune</dc:title>
    <dc:creator opf:role="aut">Frank Herbert</dc:creator>
    <dc:creator opf:role="ill">Someone Else</dc:creator>
    <dc:publisher>Chilton Books</dc:publisher>
    <dc:date>1965-08-01</dc:date>
    <dc:description>A desert planet.</dc:description>
    <dc:identifier id="uid">urn:uuid:1234-not-an-isbn</dc:identifier>
    <dc:identifier opf:scheme="ISBN">9780441013593</dc:identifier>
    <meta name="calibre:series" content="Dune Chronicles"/>
    <meta name="calibre:series_index" content="1.0"/>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="cover-img" href="images/cover.jpeg" media-type="image/jpeg"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
</package>"#;

    #[test]
    fn an_epubs_opf_metadata_is_read() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Dune.epub");
        write_epub(&p, Some(CONTAINER), Some(("OEBPS/content.opf", OPF)));

        let meta = read_epub(&p).expect("a well-formed epub must parse");
        assert_eq!(meta.title.as_deref(), Some("Dune"));
        assert_eq!(
            meta.author.as_deref(),
            Some("Frank Herbert"),
            "the FIRST dc:creator is the author; the illustrator must not win"
        );
        assert_eq!(meta.publisher.as_deref(), Some("Chilton Books"));
        assert_eq!(meta.date.as_deref(), Some("1965-08-01"));
        assert_eq!(meta.description.as_deref(), Some("A desert planet."));
        assert_eq!(
            meta.isbn.as_deref(),
            Some("9780441013593"),
            "the UUID identifier must not be stored as an ISBN"
        );
        assert_eq!(meta.series_name.as_deref(), Some("Dune Chronicles"));
        assert_eq!(
            meta.series_index,
            Some(1),
            "calibre writes 1.0; the integer part is the volume"
        );
        assert_eq!(
            meta.cover_href.as_deref(),
            Some("OEBPS/images/cover.jpeg"),
            "the cover href is relative to the OPF's directory, not the zip root"
        );

        // page_count stays None: pagination depends on the reader's viewport.
        let bm = meta.to_book_meta();
        assert_eq!(bm.format, BookFormat::Epub);
        assert_eq!(bm.page_count, None);
        assert_eq!(bm.author.as_deref(), Some("Frank Herbert"));
    }

    #[test]
    fn a_malformed_epub_still_imports_the_item() {
        let td = tempfile::tempdir().unwrap();

        // A zip with no container.xml at all.
        let p = td.path().join("NoContainer.epub");
        write_epub(&p, None, None);
        assert!(read_epub(&p).is_err(), "precondition: this cannot parse");
        let bm = read_epub_or_empty(&p);
        assert_eq!(
            bm.format,
            BookFormat::Epub,
            "V6 — the item still imports, carrying its format"
        );
        assert_eq!(bm.author, None);

        // container.xml present but truncated mid-tag.
        let p = td.path().join("Truncated.epub");
        write_epub(&p, Some("<container><rootfiles><rootfil"), None);
        let bm = read_epub_or_empty(&p);
        assert_eq!(bm.format, BookFormat::Epub);

        // A file that is not a zip at all.
        let p = td.path().join("Lying.epub");
        std::fs::write(&p, b"this is plain text").unwrap();
        assert!(matches!(read_epub(&p), Err(EpubError::NotAZip(_))));
        assert_eq!(read_epub_or_empty(&p).format, BookFormat::Epub);

        // A path that does not exist.
        let p = td.path().join("Gone.epub");
        assert!(matches!(read_epub(&p), Err(EpubError::Open(_))));
    }

    /// Every error names the offending value, not just a class. A reason that
    /// does not carry the value is another round of guessing.
    #[test]
    fn errors_carry_the_offending_value() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("BadOpfHref.epub");
        // container.xml points at an OPF that is not in the archive.
        write_epub(
            &p,
            Some(
                r#"<container><rootfiles><rootfile full-path="missing/book.opf"/></rootfiles></container>"#,
            ),
            None,
        );
        let err = read_epub(&p).expect_err("the OPF is absent");
        let msg = err.to_string();
        assert!(
            msg.contains("missing/book.opf"),
            "the error must name the entry it could not find, got: {msg}"
        );
    }

    #[test]
    fn a_cover_is_found_without_an_explicit_meta_tag() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("NoMetaCover.epub");
        let opf = r#"<package><metadata><dc:title>X</dc:title></metadata>
          <manifest>
            <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
            <item id="cover-image" href="cover.png" media-type="image/png"/>
          </manifest></package>"#;
        write_epub(&p, Some(CONTAINER), Some(("OEBPS/content.opf", opf)));
        let meta = read_epub(&p).unwrap();
        assert_eq!(
            meta.cover_href.as_deref(),
            Some("OEBPS/cover.png"),
            "an id containing \"cover\" is the fallback when no meta tag names one"
        );
    }

    #[test]
    fn a_book_with_no_cover_at_all_reports_none() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Coverless.epub");
        let opf = r#"<package><metadata><dc:title>X</dc:title></metadata>
          <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
          </package>"#;
        write_epub(&p, Some(CONTAINER), Some(("OEBPS/content.opf", opf)));
        let meta = read_epub(&p).unwrap();
        assert_eq!(
            meta.cover_href, None,
            "no image in the manifest means no cover — never a chapter file"
        );
    }

    /// An entity SPLITS the literal run quick-xml delivers, so both halves of
    /// this have to survive: the `&` itself (a named entity, which
    /// `resolve_char_ref` alone answers `None` for) and the spaces around it
    /// (which per-run trimming eats). Caught by the comic reader's tests; the
    /// same defect was here, unasserted.
    #[test]
    fn entities_survive_intact_inside_an_element() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Ampersand.epub");
        let opf = r#"<package><metadata>
            <dc:title>Sense &amp; Sensibility</dc:title>
            <dc:description>Elinor &amp; Marianne &#8212; two sisters.</dc:description>
          </metadata><manifest/></package>"#;
        write_epub(&p, Some(CONTAINER), Some(("OEBPS/content.opf", opf)));

        let meta = read_epub(&p).unwrap();
        assert_eq!(
            meta.title.as_deref(),
            Some("Sense & Sensibility"),
            "a named entity must resolve, and the spaces either side must stay"
        );
        assert_eq!(
            meta.description.as_deref(),
            Some("Elinor & Marianne — two sisters."),
            "named and numeric entities must both survive in one run"
        );
    }

    #[test]
    fn relative_hrefs_resolve_against_the_opfs_directory() {
        assert_eq!(
            resolve_relative("OEBPS/content.opf", "images/cover.jpg"),
            "OEBPS/images/cover.jpg"
        );
        assert_eq!(resolve_relative("content.opf", "cover.jpg"), "cover.jpg");
        assert_eq!(
            resolve_relative("a/b/content.opf", "../img/c.png"),
            "a/img/c.png"
        );
    }
}
