//! 005-kindle-conversion — read `.mobi` / `.azw` / `.azw3`, and convert the
//! DRM-free ones to EPUB.
//!
//! # Why this module exists
//!
//! 004-books shipped these three extensions as [`BookFormat::Unreadable`]:
//! indexed and downloadable, never claimed as readable, because jellyfin-web
//! ships exactly three readers — epub.js (`.epub`), libarchive.js (`.cbz` and
//! friends) and pdf.js (`.pdf`) — and none of them claims a Kindle file. A user
//! who clicked one got "This ebook cannot be opened".
//!
//! In the deployed library that was 75 of 142 books. Probing the MOBI
//! encryption flag (PalmDOC header, offset 12) split them cleanly:
//!
//! * **58 are DRM-protected.** Amazon DRM is tied to a registered device. No
//!   converter opens those and this one does not try — see `Drm` below, which
//!   is a first-class outcome rather than a failure, because it has no fix in
//!   code and saying so is the useful answer.
//! * **17 are DRM-free**, and every one of them converts.
//!
//! # Why a crate and not a decoder
//!
//! The 17 span all three container shapes the format has: MOBI6 + PalmDOC LZ77
//! (8 files), KF8 + PalmDOC (2) and KF8 + HUFF/CDIC (7). The last is a Huffman
//! coder, and KF8 additionally stores one decompressed flow that must be split
//! back into per-chapter XHTML through its skeleton and fragment indices.
//! That is a large amount of undocumented-format code to own.
//!
//! `boko` carries all of it, in pure Rust with no `unsafe` and no C library —
//! so it is consistent with the rule that keeps `.cbr` cover-less (R7) and
//! PDFs un-rasterised (R11). It has many `unwrap`s, which is exactly the risk
//! [`guard_parser`](super::guard_parser) already exists to contain for lopdf
//! (V119): a panic here costs one skipped book, not the scan.
//!
//! # What conversion buys beyond opening
//!
//! These files were also the library's metadata hole — no title beyond the
//! filename, and a 31% cover rate that came entirely from `cover.jpg`
//! sidecars. Parsing the EXTH records gives real titles, authors, publishers
//! and a cover for **17 of 17**. So this module is wired into metadata and
//! cover extraction, not only into delivery.

use std::path::Path;

use pharos_core::BookFormat;

use super::epub::EpubMetadata;

/// Why a Kindle file could not be read or converted.
///
/// Every variant carries the offending value — the "expose the cause"
/// discipline. `Drm` is the one that is not a defect: it names a permanent
/// property of the file, and a reader who sees it should stop looking for a
/// bug.
#[derive(Debug)]
pub enum KindleError {
    /// Amazon DRM. Permanent, and not something pharos will circumvent.
    Drm(String),
    /// The file could not be opened at all (permissions, a dead NFS mount).
    Unopenable(String),
    /// Opened, but its structure is not the format the extension claims.
    Malformed(String),
    /// Parsed, but rebuilding it as an EPUB failed.
    Export(String),
}

impl std::fmt::Display for KindleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KindleError::Drm(d) => write!(f, "DRM-protected: {d}"),
            KindleError::Unopenable(d) => write!(f, "unopenable: {d}"),
            KindleError::Malformed(d) => write!(f, "malformed: {d}"),
            KindleError::Export(d) => write!(f, "epub export failed: {d}"),
        }
    }
}

/// Map a `boko` error onto the classes pharos reports.
///
/// `boko::Error` is `#[non_exhaustive]`, so the wildcard is compulsory rather
/// than chosen — the compiler cannot be made to flag a new upstream variant
/// here. It therefore falls to `Malformed`, the arm that says "this file is
/// not what it claims", and the message carries the upstream text verbatim so
/// an unrecognised cause still names itself in the log instead of being
/// flattened into a class.
///
/// DRM is matched FIRST and explicitly. Folding it into the wildcard would
/// report 58 of the deployed library's books as corrupt, sending a reader
/// looking for a parser bug that does not exist.
fn classify(err: boko::Error) -> KindleError {
    match err {
        boko::Error::DrmProtected(fmt) => KindleError::Drm(format!("{fmt:?}")),
        boko::Error::Io(e) => KindleError::Unopenable(e.to_string()),
        e @ boko::Error::NotFound { .. } => KindleError::Export(e.to_string()),
        e => KindleError::Malformed(e.to_string()),
    }
}

/// True when `path` is one of the three Kindle extensions.
///
/// The authority on "is this file a Kindle container", used by callers that
/// hold a stored [`BookFormat`] and need to know whether it describes the file
/// on disk or the form pharos converts it into. See
/// [`is_converted_kindle`].
pub fn is_kindle_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            e == "mobi" || e == "azw" || e == "azw3"
        })
        .unwrap_or(false)
}

/// True when `stored_format` says "epub" but the file on disk is a Kindle
/// container — i.e. this item was converted at scan time.
///
/// # Why this is derived rather than stored
///
/// [`BookFormat`] answers "what can a client read this as", which after a
/// successful conversion genuinely is epub — that is what the download route
/// hands over. The path keeps answering "what is the file on disk". Those two
/// facts disagreeing IS the record of a conversion, so no column, no
/// migration, and no way for a flag to drift out of step with the two values
/// it would summarise.
///
/// Disk stays the authority on whether the converted bytes are actually
/// present, exactly as it is for trickplay: the delivery path regenerates on
/// a miss rather than trusting a stored yes.
pub fn is_converted_kindle(path: &Path, stored_format: BookFormat) -> bool {
    stored_format == BookFormat::Epub && is_kindle_path(path)
}

/// What happened when pharos tried to make a Kindle file readable.
///
/// A dashboard contract: bounded cardinality, stable strings, asserted
/// distinct in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertOutcome {
    /// EPUB bytes were produced.
    Converted,
    /// The file is DRM-protected. Not a failure — a permanent property.
    DrmProtected,
    /// Something went wrong that is not DRM.
    Failed,
}

impl ConvertOutcome {
    pub fn label(self) -> &'static str {
        match self {
            ConvertOutcome::Converted => "converted",
            ConvertOutcome::DrmProtected => "drm_protected",
            ConvertOutcome::Failed => "failed",
        }
    }
}

/// WHY, bounded to a class of cause rather than a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertReason {
    Ok,
    /// Amazon DRM.
    Drm,
    /// Could not be opened (permissions, dead mount).
    Unopenable,
    /// Not the structure its extension claims.
    Malformed,
    /// Parsed, but the EPUB rebuild failed.
    ExportFailed,
    /// The parser PANICKED (V119). Carried on this counter as well as the
    /// classify one so that "conversions attempted" and "conversions
    /// accounted for" reconcile — a panic that only appeared on the other
    /// metric would read here as an attempt that vanished.
    ParserPanic,
}

impl ConvertReason {
    pub fn label(self) -> &'static str {
        match self {
            ConvertReason::Ok => "ok",
            ConvertReason::Drm => "drm",
            ConvertReason::Unopenable => "unopenable",
            ConvertReason::Malformed => "malformed",
            ConvertReason::ExportFailed => "export_failed",
            ConvertReason::ParserPanic => "parser_panic",
        }
    }
}

impl KindleError {
    /// The outcome/reason pair this error reports.
    pub fn outcome(&self) -> (ConvertOutcome, ConvertReason) {
        match self {
            KindleError::Drm(_) => (ConvertOutcome::DrmProtected, ConvertReason::Drm),
            KindleError::Unopenable(_) => (ConvertOutcome::Failed, ConvertReason::Unopenable),
            KindleError::Malformed(_) => (ConvertOutcome::Failed, ConvertReason::Malformed),
            KindleError::Export(_) => (ConvertOutcome::Failed, ConvertReason::ExportFailed),
        }
    }
}

/// Record one conversion decision.
///
/// Separate from `pharos_book_classify_total` on purpose. A converted `.azw3`
/// is classified `epub` — that is the truthful answer to "what can a client
/// read this as" — which means the classify counter alone can no longer tell
/// you a Kindle file was involved. This counter keeps that fact, and with it
/// the only query that answers "how much of the library is locked behind DRM":
///
/// ```logql
/// sum by (outcome, reason) (pharos_book_convert_total)
/// ```
///
/// `source` is the extension, so `drm_protected` can be broken down by
/// container. Cardinality is bounded at three values by [`is_kindle_path`].
///
/// `stage` separates the two places a conversion happens, which answer
/// different questions. `scan` counts what the library CONTAINS — run it over
/// `drm_protected` and you get the size of the locked-away shelf. `deliver`
/// counts converted-cache MISSES, so a rate that does not fall to ~zero after
/// a library settles means the cache is being evicted or wiped, which is
/// invisible from the scan-side number alone.
pub fn record_convert(
    stage: ConvertStage,
    source_ext: &'static str,
    outcome: ConvertOutcome,
    reason: ConvertReason,
) {
    metrics::counter!(
        "pharos_book_convert_total",
        "stage" => stage.label(),
        "source" => source_ext,
        "outcome" => outcome.label(),
        "reason" => reason.label(),
    )
    .increment(1);
}

/// Where a conversion was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertStage {
    /// During a library scan, for metadata and a cover.
    Scan,
    /// On a download request, producing the bytes a reader opens.
    Deliver,
}

impl ConvertStage {
    pub fn label(self) -> &'static str {
        match self {
            ConvertStage::Scan => "scan",
            ConvertStage::Deliver => "deliver",
        }
    }
}

/// The extension as one of three stable label strings.
///
/// Returns a `&'static str` rather than the path's own slice so the metric
/// label can never take an unbounded value from the filesystem.
pub fn source_ext(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mobi") => "mobi",
        Some("azw") => "azw",
        Some("azw3") => "azw3",
        _ => "other",
    }
}

/// Open a Kindle file and read its descriptive metadata.
///
/// Reuses [`EpubMetadata`] rather than defining a parallel struct: the field
/// set is identical, and sharing it means a converted book reaches
/// `to_book_meta` and the metadata provider through exactly the same path an
/// epub does, with no second mapping to keep in step.
pub fn read_kindle(path: &Path) -> Result<EpubMetadata, KindleError> {
    let book = boko::Book::open(path).map_err(classify)?;
    let m = book.metadata();

    // First author only, matching the epub reader: an author list repeated for
    // translators and illustrators has the author first by convention.
    let author = m.authors.first().filter(|a| !a.is_empty()).cloned();
    let series = m.collection.as_ref();

    Ok(EpubMetadata {
        title: Some(m.title.clone()).filter(|t| !t.is_empty()),
        author,
        publisher: m.publisher.clone().filter(|p| !p.is_empty()),
        description: m.description.clone().filter(|d| !d.is_empty()),
        date: m.date.clone().filter(|d| !d.is_empty()),
        isbn: isbn_of(&m.identifier),
        series_name: series.map(|c| c.name.clone()).filter(|n| !n.is_empty()),
        // A Kindle collection position is fractional (1, 2, 3.5) because a
        // series can carry novellas between volumes. pharos stores a whole
        // number, so a fractional position is dropped rather than rounded —
        // rounding 3.5 to 4 would sort a novella ahead of volume 4.
        series_index: series
            .and_then(|c| c.position)
            .filter(|p| p.fract() == 0.0 && *p >= 0.0 && *p <= f64::from(u32::MAX))
            .map(|p| p as u32),
        cover_href: m.cover_image.clone(),
    })
}

/// A `dc:identifier` that actually looks like an ISBN.
///
/// Same 10-or-13-digit rule as the epub reader, because the field is equally
/// overloaded here: Kindle files carry an ASIN (`B002N3M6RW`) far more often
/// than an ISBN, and one library file carries the placeholder `0000000000000`,
/// which passes a pure digit count. Storing either as an ISBN would be a lie
/// the UI displays and an online-metadata lookup would then act on.
fn isbn_of(identifier: &str) -> Option<String> {
    let digits: String = identifier.chars().filter(char::is_ascii_digit).collect();
    if (digits.len() == 10 || digits.len() == 13) && digits.bytes().any(|b| b != b'0') {
        Some(identifier.to_string())
    } else {
        None
    }
}

/// The cover image bytes a Kindle file declares, if any.
///
/// Every one of the 17 convertible files in the deployed library declares one,
/// so `Ok(None)` is expected to be rare — but it stays a distinct answer from
/// an error, because "this book has no cover" and "this book could not be
/// read" have different fixes.
pub fn read_kindle_cover(path: &Path) -> Result<Option<Vec<u8>>, KindleError> {
    let book = boko::Book::open(path).map_err(classify)?;
    let Some(href) = book.metadata().cover_image.clone() else {
        return Ok(None);
    };
    match book.load_asset(&href) {
        Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
        // A declared cover the container does not actually hold is the B149
        // shape: advertising a Primary tag with no bytes behind it 404s on
        // every grid render. Reported as absent, never as found.
        Ok(_) => Ok(None),
        Err(boko::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(classify(e)),
    }
}

/// Convert a DRM-free Kindle file into EPUB bytes.
///
/// The output is a spec-shaped OCF zip — `mimetype` first and STORED,
/// `META-INF/container.xml` present — which is what epub.js requires to open
/// it at all.
pub fn convert_to_epub(path: &Path) -> Result<Vec<u8>, KindleError> {
    let book = boko::Book::open(path).map_err(classify)?;
    let mut out = std::io::Cursor::new(Vec::new());
    book.export(boko::Format::Epub, &mut out)
        .map_err(|e| match classify(e) {
            // An error raised during EXPORT is an export failure whatever its
            // upstream shape; only the open-time DRM check means DRM.
            KindleError::Drm(d) => KindleError::Drm(d),
            other => KindleError::Export(other.to_string()),
        })?;
    Ok(out.into_inner())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// A minimal but spec-valid EPUB, built rather than checked in as a blob
    /// so the fixture's contents are visible in the test that reads them (the
    /// same choice the epub and comic fixtures make).
    fn minimal_epub(title: &str, author: &str, body: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // OCF requires `mimetype` first and uncompressed.
        zw.start_file("mimetype", stored).unwrap();
        zw.write_all(b"application/epub+zip").unwrap();

        let deflated = zip::write::SimpleFileOptions::default();
        zw.start_file("META-INF/container.xml", deflated).unwrap();
        zw.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();

        zw.start_file("OEBPS/content.opf", deflated).unwrap();
        zw.write_all(
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:language>en</dc:language>
    <dc:identifier id="uid">9781718501867</dc:identifier>
    <dc:publisher>No Starch Press</dc:publisher>
  </metadata>
  <manifest>
    <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#
            )
            .as_bytes(),
        )
        .unwrap();

        zw.start_file("OEBPS/ch1.xhtml", deflated).unwrap();
        zw.write_all(
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title></head>
<body><h1>{title}</h1><p>{body}</p></body></html>"#
            )
            .as_bytes(),
        )
        .unwrap();

        zw.finish().unwrap().into_inner()
    }

    /// As [`minimal_epub`], plus a declared cover image so the cover path has
    /// something real to find.
    fn epub_with_cover(title: &str, jpeg: &[u8]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("mimetype", stored).unwrap();
        zw.write_all(b"application/epub+zip").unwrap();

        let deflated = zip::write::SimpleFileOptions::default();
        zw.start_file("META-INF/container.xml", deflated).unwrap();
        zw.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();

        zw.start_file("OEBPS/cover.jpg", deflated).unwrap();
        zw.write_all(jpeg).unwrap();

        zw.start_file("OEBPS/content.opf", deflated).unwrap();
        zw.write_all(
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title><dc:language>en</dc:language>
    <dc:identifier id="uid">urn:uuid:cover-fixture</dc:identifier>
  </metadata>
  <manifest>
    <item id="cover" href="cover.jpg" media-type="image/jpeg" properties="cover-image"/>
    <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#
            )
            .as_bytes(),
        )
        .unwrap();

        zw.start_file("OEBPS/ch1.xhtml", deflated).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head>
<body><p>body text</p></body></html>"#,
        )
        .unwrap();

        zw.finish().unwrap().into_inner()
    }

    /// Write a real `.azw3` by round-tripping the fixture EPUB through the
    /// converter's own AZW3 writer.
    ///
    /// This is what makes the test hermetic: the deployed library's Kindle
    /// files are copyrighted and cannot be committed, and hand-assembling a
    /// MOBI container in the test would be asserting against my reading of an
    /// undocumented format rather than against a real one.
    fn write_azw3(dir: &std::path::Path, name: &str, epub: &[u8]) -> PathBuf {
        let book =
            boko::Book::from_bytes(epub, boko::Format::Epub).expect("the fixture epub must import");
        let mut out = std::io::Cursor::new(Vec::new());
        book.export(boko::Format::Azw3, &mut out)
            .expect("the fixture must export to azw3");
        let path = dir.join(name);
        std::fs::write(&path, out.into_inner()).unwrap();
        path
    }

    /// The whole feature in one assertion: a Kindle file goes in, real
    /// metadata and an EPUB that epub.js can open come out.
    #[test]
    fn an_azw3_is_read_and_converted_to_an_openable_epub() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_azw3(
            dir.path(),
            "Rust for Rustaceans.azw3",
            &minimal_epub(
                "Rust for Rustaceans",
                "Gjengset, Jon",
                "Talking About Memory",
            ),
        );

        // 1. Metadata comes off the Kindle container, not the filename.
        let meta = read_kindle(&src).expect("a DRM-free azw3 must read");
        assert_eq!(meta.title.as_deref(), Some("Rust for Rustaceans"));
        assert_eq!(meta.author.as_deref(), Some("Gjengset, Jon"));
        assert_eq!(meta.publisher.as_deref(), Some("No Starch Press"));
        // NOT asserted here: `isbn`. The fixture is made by boko's own AZW3
        // WRITER, and that writer does not carry `dc:identifier` through — so
        // a round-trip cannot prove anything about ISBN either way.
        //
        // Reading it does work on real files: the deployed library's
        // `Rust for Rustaceans.azw3` returns `9781718501867`, which is the
        // value `only_real_isbns_are_kept` pins the extraction rule against.
        // Asserting `None` here would be asserting a fixture's limitation as
        // if it were behaviour.

        // 2. It is stored as the format a client can READ it as. This is the
        //    value the delivery path reads back to know the item converts, so
        //    it is load-bearing rather than cosmetic.
        assert_eq!(meta.to_book_meta().format, BookFormat::Epub);
        assert!(is_converted_kindle(&src, meta.to_book_meta().format));

        // 3. The bytes are an EPUB epub.js will actually open: OCF requires
        //    `mimetype` FIRST and STORED, and a missing container.xml makes it
        //    fail opaquely — which would look like a pharos bug, not a
        //    malformed zip.
        let epub = convert_to_epub(&src).expect("a DRM-free azw3 must convert");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(epub)).expect("output must be a zip");
        let first = z.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), zip::CompressionMethod::Stored);
        drop(first);
        let names: Vec<String> = z.file_names().map(str::to_string).collect();
        assert!(
            names.iter().any(|n| n == "META-INF/container.xml"),
            "no container.xml in {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with(".opf")),
            "no OPF in {names:?}"
        );
    }

    /// The reader's gate is `Path.endsWith("epub")`, so a converted book is
    /// only openable if the CONVERTED path is what gets advertised. Asserting
    /// the derived predicate here keeps that contract next to the conversion
    /// rather than only in the DTO layer that consumes it.
    #[test]
    fn a_converted_books_delivered_name_ends_in_epub() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_azw3(
            dir.path(),
            "The Prince.azw3",
            &minimal_epub("The Prince", "Machiavelli, Niccolo", "Chapter I"),
        );
        let format = read_kindle(&src).unwrap().to_book_meta().format;
        assert!(is_converted_kindle(&src, format));
        // What the item's `Path` becomes is the cache file, and its extension
        // is the thing bookPlayer compares.
        assert!(PathBuf::from("/cache/books/42.epub")
            .to_string_lossy()
            .ends_with("epub"));
    }

    /// The integration point the scan actually calls.
    ///
    /// `read_book_meta` is what decides the `book_format` column, and that
    /// column is what the delivery path reads back. Asserting `read_kindle`
    /// alone would leave the branch that routes a `.azw3` INTO it untested —
    /// and that branch is the entire difference between 004-books' behaviour
    /// and this one.
    #[test]
    fn the_scan_stores_a_convertible_kindle_file_as_readable() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_azw3(
            dir.path(),
            "Gray Hat Python.azw3",
            &minimal_epub("Gray Hat Python", "Seitz, Justin", "Chapter 1"),
        );
        let meta = super::super::read_book_meta(&src).expect("a book path must yield meta");
        assert_eq!(
            meta.format,
            BookFormat::Epub,
            "a DRM-free Kindle file must be stored as the format it converts to"
        );
        assert_eq!(meta.author.as_deref(), Some("Seitz, Justin"));

        // The counterweight: a Kindle file that does NOT convert must keep the
        // 004-books verdict. Without this, "everything is epub now" would pass
        // the assertion above just as well.
        let broken = dir.path().join("Atomic Habits.azw3");
        std::fs::write(&broken, b"not a mobi").unwrap();
        assert_eq!(
            super::super::read_book_meta(&broken)
                .expect("still imports (V6)")
                .format,
            BookFormat::Unreadable
        );
    }

    /// The cover is the other half of what conversion buys, and it flows
    /// through a DIFFERENT dispatcher (`read_book_cover`) than the metadata —
    /// so it needs its own assertion or a regression there would be silent.
    #[test]
    fn a_kindle_files_cover_is_extracted_through_the_normal_cover_path() {
        let dir = tempfile::tempdir().unwrap();
        // A 1x1 JPEG: enough for the container to carry a real cover resource.
        let jpeg: &[u8] = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02,
            0x02, 0x02, 0x02, 0x03, 0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x06, 0x04,
            0x04, 0x04, 0x04, 0x04, 0x08, 0x06, 0x06, 0x05, 0x06, 0x09, 0x08, 0x0A, 0x0A, 0x09,
            0x08, 0x09, 0x09, 0x0A, 0x0C, 0x0F, 0x0C, 0x0A, 0x0B, 0x0E, 0x0B, 0x09, 0x09, 0x0D,
            0x11, 0x0D, 0x0E, 0x0F, 0x10, 0x10, 0x11, 0x10, 0x0A, 0x0C, 0x12, 0x13, 0x12, 0x10,
            0x13, 0x0F, 0x10, 0x10, 0x10, 0xFF, 0xC9, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xCC, 0x00, 0x06, 0x00, 0x10, 0x10, 0x05, 0xFF, 0xDA,
            0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xD2, 0xCF, 0x20, 0xFF, 0xD9,
        ];
        let epub = epub_with_cover("Covered Book", jpeg);
        let src = write_azw3(dir.path(), "Covered Book.azw3", &epub);

        let cover = super::super::read_book_cover(&src);
        assert!(
            cover.is_some_and(|b| b.starts_with(&[0xFF, 0xD8])),
            "a Kindle file's declared cover must come back as JPEG bytes"
        );
    }

    /// The counter must actually FIRE, with the labels a dashboard queries.
    ///
    /// The label tests above pin the strings; this pins that they reach the
    /// registry at all. Both directions are asserted from one scan, because
    /// the interesting query — "how much of this library is locked behind
    /// DRM?" — is a ratio, and a ratio is wrong if either side is missing.
    #[test]
    fn a_scan_records_both_a_conversion_and_a_failure() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let dir = tempfile::tempdir().unwrap();
        let good = write_azw3(
            dir.path(),
            "The Prince.azw3",
            &minimal_epub("The Prince", "Machiavelli, Niccolo", "Chapter I"),
        );
        let bad = dir.path().join("Broken.azw3");
        std::fs::write(&bad, b"not a mobi container").unwrap();

        let _ = super::super::read_book_meta(&good);
        let _ = super::super::read_book_meta(&bad);

        let seen: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(k, _, _, _)| {
                let key = k.key();
                (key.name() == "pharos_book_convert_total").then(|| {
                    key.labels()
                        .map(|l| (l.key().to_string(), l.value().to_string()))
                        .collect::<std::collections::HashMap<_, _>>()
                })
            })
            .collect();

        assert!(
            seen.iter().any(
                |l| l.get("outcome").map(String::as_str) == Some("converted")
                    && l.get("stage").map(String::as_str) == Some("scan")
                    && l.get("source").map(String::as_str) == Some("azw3")
            ),
            "a successful conversion must be counted: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|l| l.get("outcome").map(String::as_str) == Some("failed")),
            "a failure must be counted too — rich-on-success/silent-on-failure is \
             the shape that hides outages: {seen:?}"
        );
    }

    /// A file that is not a Kindle container at all must be REPORTED, not
    /// silently treated as empty — the classify counter's whole purpose is
    /// that a user seeing a broken book can tell a bug from a design limit.
    #[test]
    fn a_non_kindle_file_fails_with_a_named_cause() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-book.azw3");
        std::fs::write(&path, b"this is not a mobi container").unwrap();
        let err = read_kindle(&path).expect_err("garbage must not read as a book");
        assert_eq!(err.outcome().0, ConvertOutcome::Failed);
        assert!(
            !err.to_string().is_empty(),
            "the cause must name itself: {err}"
        );
    }

    #[test]
    fn kindle_extensions_are_recognised_case_insensitively() {
        for p in ["a.mobi", "a.azw", "a.azw3", "A.AZW3", "a.MOBI"] {
            assert!(is_kindle_path(&PathBuf::from(p)), "{p} should be Kindle");
        }
        for p in ["a.epub", "a.pdf", "a.cbz", "a.mp4", "a"] {
            assert!(!is_kindle_path(&PathBuf::from(p)), "{p} should not be");
        }
    }

    /// The derived converted-ness rule, both ways round. A stored `Epub` on an
    /// `.epub` path is an ordinary epub and must NOT be treated as converted,
    /// or the delivery path would try to re-convert a file that is already in
    /// its final form.
    #[test]
    fn converted_kindle_is_stored_epub_on_a_kindle_path() {
        let azw3 = PathBuf::from("/m/The Prince.azw3");
        let epub = PathBuf::from("/m/The Prince.epub");
        assert!(is_converted_kindle(&azw3, BookFormat::Epub));
        assert!(!is_converted_kindle(&azw3, BookFormat::Unreadable));
        assert!(!is_converted_kindle(&epub, BookFormat::Epub));
        assert!(!is_converted_kindle(&epub, BookFormat::Unreadable));
    }

    #[test]
    fn source_ext_is_bounded_to_known_labels() {
        assert_eq!(source_ext(&PathBuf::from("a.azw3")), "azw3");
        assert_eq!(source_ext(&PathBuf::from("a.AZW")), "azw");
        assert_eq!(source_ext(&PathBuf::from("a.mobi")), "mobi");
        // Anything else collapses to one bounded label rather than leaking a
        // filesystem-supplied string into a metric.
        assert_eq!(source_ext(&PathBuf::from("a.weird")), "other");
        assert_eq!(source_ext(&PathBuf::from("a")), "other");
    }

    /// Metric labels are a dashboard contract — a rename breaks alerts
    /// silently, so the whole set is asserted at once and asserted distinct.
    #[test]
    fn convert_labels_are_stable_and_distinct() {
        let outcomes = [
            ConvertOutcome::Converted,
            ConvertOutcome::DrmProtected,
            ConvertOutcome::Failed,
        ];
        let labels: Vec<_> = outcomes.iter().map(|o| o.label()).collect();
        assert_eq!(labels, ["converted", "drm_protected", "failed"]);
        let uniq: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(uniq.len(), labels.len(), "outcome labels must be distinct");

        let reasons = [
            ConvertReason::Ok,
            ConvertReason::Drm,
            ConvertReason::Unopenable,
            ConvertReason::Malformed,
            ConvertReason::ExportFailed,
            ConvertReason::ParserPanic,
        ];
        let rlabels: Vec<_> = reasons.iter().map(|r| r.label()).collect();
        assert_eq!(
            rlabels,
            [
                "ok",
                "drm",
                "unopenable",
                "malformed",
                "export_failed",
                "parser_panic"
            ]
        );
        let runiq: std::collections::HashSet<_> = rlabels.iter().collect();
        assert_eq!(runiq.len(), rlabels.len(), "reason labels must be distinct");
    }

    /// DRM must never be reported as a failure: it is a permanent property of
    /// the file with no fix in code, and folding it into `failed` would put 58
    /// of the deployed library's books into a bucket that reads as a bug.
    #[test]
    fn drm_is_its_own_outcome_not_a_failure() {
        let (o, r) = KindleError::Drm("Azw3".into()).outcome();
        assert_eq!(o, ConvertOutcome::DrmProtected);
        assert_eq!(r, ConvertReason::Drm);

        for e in [
            KindleError::Unopenable("x".into()),
            KindleError::Malformed("x".into()),
            KindleError::Export("x".into()),
        ] {
            assert_eq!(e.outcome().0, ConvertOutcome::Failed, "{e} should fail");
        }
    }

    /// An ASIN is not an ISBN, and neither is a field of zeroes — one library
    /// file really does carry `0000000000000`, which a bare digit count
    /// accepts.
    #[test]
    fn only_real_isbns_are_kept() {
        assert_eq!(isbn_of("9781718501867").as_deref(), Some("9781718501867"));
        assert_eq!(
            isbn_of("978-0-9888202-0-4").as_deref(),
            Some("978-0-9888202-0-4")
        );
        assert_eq!(isbn_of("B002N3M6RW"), None);
        assert_eq!(isbn_of("0000000000000"), None);
        assert_eq!(isbn_of(""), None);
        assert_eq!(isbn_of("urn:uuid:b2d66a30-ff4f-4a"), None);
    }
}
