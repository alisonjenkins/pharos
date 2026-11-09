//! Whole-file integrity check for books — the book half of the library
//! integrity sweep.
//!
//! The scan reads a book's metadata out of its index: a zip's central
//! directory, a PDF's xref, a Kindle header. None of that touches the bytes of
//! the pages, so an archive with a corrupt entry in the middle imports looking
//! healthy and fails only when a reader turns to that page. This module reads
//! every part of the file and lets each format's own checksum or structure say
//! whether it is intact:
//!
//! * zip (epub, cbz): every entry is inflated to the end, which is where the
//!   `zip` crate checks the entry's CRC-32.
//! * tar (cbt): every entry is read to the end; tar has no data checksum, so
//!   this finds truncation and broken headers, not flipped bits.
//! * 7z (cb7): every entry is decompressed, which checks its CRC.
//! * PDF: the document is parsed and every page's content streams are
//!   decompressed.
//! * Kindle (mobi, azw, azw3): the book is opened and exported to EPUB in
//!   memory, which reads every text record.
//!
//! A `.cbr` (rar, no pure-Rust reader, R7) and a DRM-locked Kindle file are not
//! checkable at all, and say so as [`BookCheck::NotCheckable`] rather than
//! passing as clean.

use std::io::Read;
use std::path::Path;

use super::comic::ComicContainer;

/// What reading a book end to end found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookIntegrity {
    /// Parts read successfully: archive entries or PDF pages.
    pub parts: u64,
    /// Parts that failed to read.
    pub faults: u64,
    /// The first failing part and the reader's own words for it, e.g.
    /// `OEBPS/ch03.xhtml: Invalid checksum`.
    pub first_fault: Option<String>,
    /// Whether the whole file was walked. `false` means its index itself would
    /// not open, so nothing past it could be read.
    pub complete: bool,
}

/// The outcome of a book check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookCheck {
    Checked(BookIntegrity),
    /// The format cannot be verified by anything in this workspace. Carries
    /// the reason, so "not checked" is never mistaken for "clean".
    NotCheckable(String),
}

/// Read the book at `path` end to end.
///
/// `Err` only when the file cannot be opened at all (gone, permissions, a dead
/// mount): that is a fact about the storage, not a verdict about the file.
pub fn check(path: &Path) -> std::io::Result<BookCheck> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if let Some(container) = ComicContainer::for_path(path) {
        return match container {
            ComicContainer::Zip => check_zip(path),
            ComicContainer::Tar => check_tar(path).map(BookCheck::Checked),
            ComicContainer::SevenZ => check_7z(path),
            ComicContainer::Rar => Ok(BookCheck::NotCheckable(
                "rar archives have no reader in pharos".into(),
            )),
        };
    }
    match ext.as_str() {
        "epub" | "zip" => check_zip(path),
        "pdf" => check_pdf(path),
        "mobi" | "azw" | "azw3" => check_kindle(path),
        other => Ok(BookCheck::NotCheckable(format!(
            "no integrity reader for .{other}"
        ))),
    }
}

/// Bytes of an entry read per call. Only bounds the buffer; the entry is read
/// to its end either way.
const CHUNK: usize = 64 * 1024;

fn drain(mut r: impl Read) -> std::io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    while r.read(&mut buf)? != 0 {}
    Ok(())
}

/// The index would not open: one fault, nothing walked.
fn unreadable(detail: impl std::fmt::Display) -> BookIntegrity {
    BookIntegrity {
        parts: 0,
        faults: 1,
        first_fault: Some(detail.to_string()),
        complete: false,
    }
}

impl BookIntegrity {
    fn ok(&mut self) {
        self.parts = self.parts.saturating_add(1);
    }

    fn fault(&mut self, part: &str, detail: impl std::fmt::Display) {
        self.faults = self.faults.saturating_add(1);
        if self.first_fault.is_none() {
            self.first_fault = Some(format!("{part}: {detail}"));
        }
    }
}

/// A zip error that says the reader here cannot decode the entry, not that the
/// bytes are bad: encryption, or a method this build leaves out (only deflate
/// is compiled in). Calling those damaged would send someone to re-acquire a
/// good book.
fn zip_not_checkable(e: &zip::result::ZipError) -> bool {
    use zip::result::ZipError;
    matches!(
        e,
        ZipError::UnsupportedArchive(_)
            | ZipError::CompressionMethodNotSupported(_)
            | ZipError::InvalidPassword
    )
}

fn check_zip(path: &Path) -> std::io::Result<BookCheck> {
    let file = std::fs::File::open(path)?;
    let mut zip = match zip::ZipArchive::new(file) {
        Ok(z) => z,
        Err(e) if zip_not_checkable(&e) => return Ok(BookCheck::NotCheckable(format!("zip: {e}"))),
        Err(e) => {
            return Ok(BookCheck::Checked(unreadable(format!(
                "zip directory: {e}"
            ))))
        }
    };
    let mut out = BookIntegrity {
        complete: true,
        ..Default::default()
    };
    for i in 0..zip.len() {
        match zip.by_index(i) {
            Ok(entry) => {
                let name = entry.name().map(|n| n.into_owned()).unwrap_or_default();
                // The entry opens either way; the refusal comes on read, as
                // an I/O error indistinguishable from a bad CRC.
                let method = entry.compression();
                if entry.encrypted()
                    || !matches!(
                        method,
                        zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
                    )
                {
                    return Ok(BookCheck::NotCheckable(format!(
                        "zip entry {name}: {method:?}{} is not readable here",
                        if entry.encrypted() {
                            ", encrypted,"
                        } else {
                            ""
                        }
                    )));
                }
                match drain(entry) {
                    Ok(()) => out.ok(),
                    Err(e) => out.fault(&name, e),
                }
            }
            Err(e) if zip_not_checkable(&e) => {
                return Ok(BookCheck::NotCheckable(format!("zip entry {i}: {e}")))
            }
            Err(e) => out.fault(&format!("entry {i}"), e),
        }
    }
    Ok(BookCheck::Checked(out))
}

fn check_tar(path: &Path) -> std::io::Result<BookIntegrity> {
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);
    let entries = match archive.entries() {
        Ok(e) => e,
        Err(e) => return Ok(unreadable(format!("tar header: {e}"))),
    };
    let mut out = BookIntegrity {
        complete: true,
        ..Default::default()
    };
    for entry in entries {
        match entry {
            Ok(entry) => {
                let name = entry
                    .path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match drain(entry) {
                    Ok(()) => out.ok(),
                    Err(e) => out.fault(&name, e),
                }
            }
            // A broken header loses the position of everything after it, so
            // the walk stops here rather than reading garbage as entries.
            Err(e) => {
                out.fault("tar header", e);
                break;
            }
        }
    }
    Ok(out)
}

/// A 7z error that names a limit of this reader (a password, a codec left
/// out of the build), not damage in the file.
fn sevenz_not_checkable(e: &sevenz_rust2::Error) -> bool {
    use sevenz_rust2::Error as E;
    matches!(
        e,
        E::PasswordRequired
            | E::MaybeBadPassword(_)
            | E::UnsupportedCompressionMethod(_)
            | E::Unsupported(_)
            | E::UnsupportedVersion { .. }
            | E::ExternalUnsupported
            | E::MaxMemLimited { .. }
    )
}

fn check_7z(path: &Path) -> std::io::Result<BookCheck> {
    let file = std::fs::File::open(path)?;
    let mut reader = match sevenz_rust2::ArchiveReader::new(file, sevenz_rust2::Password::empty()) {
        Ok(r) => r,
        Err(e) if sevenz_not_checkable(&e) => {
            return Ok(BookCheck::NotCheckable(format!("7z: {e}")))
        }
        Err(e) => return Ok(BookCheck::Checked(unreadable(format!("7z header: {e}")))),
    };
    let mut out = BookIntegrity {
        complete: true,
        ..Default::default()
    };
    let walked = reader.for_each_entries(|entry, rd| {
        match drain(rd) {
            Ok(()) => out.ok(),
            Err(e) => out.fault(&entry.name, e),
        }
        Ok(true)
    });
    match walked {
        Ok(()) => {}
        Err(e) if sevenz_not_checkable(&e) => {
            return Ok(BookCheck::NotCheckable(format!("7z: {e}")))
        }
        // A solid 7z block that fails mid-stream aborts the walk; the entries
        // after it were never reached.
        Err(e) => out.fault("7z stream", e),
    }
    Ok(BookCheck::Checked(out))
}

/// The largest PDF the check loads. lopdf reads the whole document into
/// memory, and a scanned book past this is better left unchecked than allowed
/// to push the server toward its memory limit.
pub const MAX_PDF_BYTES: u64 = 512 * 1024 * 1024;

fn check_pdf(path: &Path) -> std::io::Result<BookCheck> {
    // Open first, so a missing file is a storage error and not a verdict.
    let size = std::fs::File::open(path)?.metadata()?.len();
    if size > MAX_PDF_BYTES {
        return Ok(BookCheck::NotCheckable(format!(
            "{size} bytes is over the {MAX_PDF_BYTES}-byte limit for a PDF check"
        )));
    }
    let doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            return Ok(BookCheck::Checked(unreadable(format!(
                "pdf structure: {e}"
            ))))
        }
    };
    // Its streams are ciphertext until decrypted, so inflating them proves
    // nothing, and passing every page unread would call it clean.
    if doc.is_encrypted() {
        return Ok(BookCheck::NotCheckable("encrypted PDF".into()));
    }
    let mut out = BookIntegrity {
        complete: true,
        ..Default::default()
    };
    for (number, page) in doc.get_pages() {
        let read =
            doc.get_page_contents(page)
                .into_iter()
                .try_for_each(|id| -> Result<(), String> {
                    let stream = doc
                        .get_object(id)
                        .and_then(lopdf::Object::as_stream)
                        .map_err(|e| e.to_string())?;
                    let first = stream.filters().ok().and_then(|f| f.first().copied());
                    if first == Some(b"FlateDecode".as_slice()) {
                        inflate(&stream.content).map_err(|e| format!("FlateDecode: {e}"))?;
                    }
                    Ok(())
                });
        match read {
            Ok(()) => out.ok(),
            Err(e) => out.fault(&format!("page {number}"), e),
        }
    }
    Ok(BookCheck::Checked(out))
}

/// Inflate a zlib stream to nowhere. Streaming into a sink, so a decompression
/// bomb costs time and never memory; the adler-32 trailer is checked at the end.
fn inflate(bytes: &[u8]) -> std::io::Result<u64> {
    std::io::copy(
        &mut flate2::read::ZlibDecoder::new(bytes),
        &mut std::io::sink(),
    )
}

fn check_kindle(path: &Path) -> std::io::Result<BookCheck> {
    use super::kindle::{convert_to_epub, KindleError};
    std::fs::File::open(path)?;
    Ok(match convert_to_epub(path) {
        Ok(_) => BookCheck::Checked(BookIntegrity {
            parts: 1,
            complete: true,
            ..Default::default()
        }),
        Err(KindleError::Drm(d)) => BookCheck::NotCheckable(format!("DRM-protected: {d}")),
        // The file opened a moment ago, so an I/O error inside boko is a
        // short read of its records (a truncated file), not the storage.
        // Returning `Err` would retry it every pass without ever listing it.
        Err(KindleError::Unopenable(d)) => {
            BookCheck::Checked(unreadable(format!("read failed: {d}")))
        }
        Err(KindleError::Malformed(d)) => BookCheck::Checked(unreadable(format!("malformed: {d}"))),
        // An export failure can be a gap in the converter as easily as damage
        // in the file, and a false "damaged" sends someone to re-acquire a
        // good book.
        Err(KindleError::Export(d)) => {
            BookCheck::NotCheckable(format!("could not be converted to check: {d}"))
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])], method: zip::CompressionMethod) {
        let file = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
    }

    /// Flip one byte inside `needle`'s stored body, leaving every header and
    /// the central directory intact — the shape the metadata scan cannot see.
    fn corrupt_body(path: &Path, needle: &[u8]) {
        let mut bytes = std::fs::read(path).unwrap();
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the body must be stored verbatim");
        bytes[at + needle.len() / 2] ^= 0xff;
        std::fs::write(path, bytes).unwrap();
    }

    const PAGE: &[u8] = b"page-two-image-bytes-that-are-long-enough-to-hit";

    #[test]
    fn a_healthy_comic_zip_reads_clean() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.cbz");
        write_zip(
            &path,
            &[("001.jpg", b"one"), ("002.jpg", PAGE)],
            zip::CompressionMethod::Stored,
        );
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a cbz is checkable");
        };
        assert_eq!(r.parts, 2);
        assert_eq!(r.faults, 0);
        assert!(r.complete);
    }

    /// The case the scan misses: the central directory is fine, so the book
    /// imports with the right page count, but page two's bytes fail their CRC.
    #[test]
    fn a_flipped_byte_inside_one_page_is_found_and_named() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.cbz");
        write_zip(
            &path,
            &[("001.jpg", b"one"), ("002.jpg", PAGE)],
            zip::CompressionMethod::Stored,
        );
        corrupt_body(&path, PAGE);
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a cbz is checkable");
        };
        assert_eq!(r.faults, 1, "{r:?}");
        assert_eq!(r.parts, 1, "page one still reads");
        let fault = r.first_fault.unwrap();
        assert!(fault.starts_with("002.jpg: "), "names the entry: {fault}");
    }

    #[test]
    fn an_epub_is_checked_as_a_zip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.epub");
        write_zip(
            &path,
            &[
                ("mimetype", b"application/epub+zip"),
                ("OEBPS/ch1.xhtml", PAGE),
            ],
            zip::CompressionMethod::Deflated,
        );
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("an epub is checkable");
        };
        assert_eq!((r.parts, r.faults), (2, 0));
    }

    #[test]
    fn a_truncated_zip_is_unreadable_not_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.epub");
        write_zip(&path, &[("a", PAGE)], zip::CompressionMethod::Stored);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("an epub is checkable");
        };
        assert!(!r.complete);
        assert_eq!(r.faults, 1);
        assert!(r.first_fault.unwrap().starts_with("zip directory: "));
    }

    #[test]
    fn a_truncated_tar_is_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.cbt");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut b = tar::Builder::new(file);
            let body = vec![7u8; 4096];
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_cksum();
            b.append_data(&mut h, "001.jpg", body.as_slice()).unwrap();
            b.finish().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        // Cut inside the entry's body: header intact, data missing.
        std::fs::write(&path, &bytes[..512 + 1000]).unwrap();
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a cbt is checkable");
        };
        assert_eq!(r.faults, 1, "{r:?}");
    }

    #[test]
    fn a_healthy_7z_reads_clean() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("001.jpg"), PAGE).unwrap();
        let path = dir.path().join("book.cb7");
        sevenz_rust2::compress_to_path(&src, &path).unwrap();
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a cb7 is checkable");
        };
        assert_eq!(r.faults, 0, "{r:?}");
        assert!(r.parts >= 1);
    }

    #[test]
    fn a_zip_this_build_cannot_decode_is_not_checkable_rather_than_damaged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.cbz");
        write_zip(&path, &[("001.jpg", PAGE)], zip::CompressionMethod::Stored);
        // Rewrite the method field (stored = 0) in both the local header and
        // the central directory to 12 (bzip2), which this build leaves out.
        let mut bytes = std::fs::read(&path).unwrap();
        for sig in [&b"PK\x03\x04"[..], &b"PK\x01\x02"[..]] {
            let at = bytes.windows(4).position(|w| w == sig).unwrap();
            let method = at + if sig[2] == 3 { 8 } else { 10 };
            bytes[method] = 12;
        }
        std::fs::write(&path, bytes).unwrap();
        assert!(
            matches!(check(&path).unwrap(), BookCheck::NotCheckable(_)),
            "an unsupported method is a limit of this reader, not damage"
        );
    }

    #[test]
    fn a_truncated_kindle_file_is_unreadable_not_retried_forever() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.azw3");
        std::fs::write(&path, b"BOOKMOBI").unwrap();
        match check(&path) {
            Ok(BookCheck::Checked(r)) => assert!(r.faults > 0, "{r:?}"),
            Ok(BookCheck::NotCheckable(why)) => panic!("an 8-byte azw3 is not intact: {why}"),
            Err(e) => panic!("an opened file must get a verdict, not a retry: {e}"),
        }
    }

    #[test]
    fn a_rar_comic_is_not_checkable_rather_than_clean() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.cbr");
        std::fs::write(&path, b"Rar!").unwrap();
        assert!(matches!(check(&path).unwrap(), BookCheck::NotCheckable(_)));
    }

    /// One page whose content stream carries `filter` and `body` verbatim.
    fn write_pdf(path: &Path, filter: Option<&str>, body: Vec<u8>) {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut dict = dictionary! {};
        if let Some(f) = filter {
            dict.set("Filter", Object::Name(f.as_bytes().to_vec()));
        }
        let mut stream = Stream::new(dict, body);
        stream.allows_compression = false;
        let content_id = doc.add_object(stream);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn a_healthy_pdf_reads_every_page() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.pdf");
        write_pdf(&path, None, b"BT /F1 12 Tf (hi) Tj ET".to_vec());
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a pdf is checkable");
        };
        assert_eq!((r.parts, r.faults, r.complete), (1, 0, true), "{r:?}");
    }

    #[test]
    fn a_healthy_compressed_pdf_page_is_not_called_damaged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.pdf");
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b"BT /F1 12 Tf (hi) Tj ET").unwrap();
        write_pdf(&path, Some("FlateDecode"), z.finish().unwrap());
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a pdf is checkable");
        };
        assert_eq!((r.parts, r.faults), (1, 0), "{r:?}");
    }

    /// The xref and page tree are intact, so the scan counts the page; only
    /// inflating its content stream shows the bytes are garbage.
    #[test]
    fn a_pdf_page_whose_stream_will_not_inflate_is_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.pdf");
        write_pdf(
            &path,
            Some("FlateDecode"),
            vec![0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3],
        );
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a pdf is checkable");
        };
        assert_eq!(r.faults, 1, "{r:?}");
        assert!(r.first_fault.unwrap().starts_with("page 1: "));
    }

    #[test]
    fn a_broken_pdf_is_unreadable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("book.pdf");
        std::fs::write(&path, b"%PDF-1.4\nthis is not a pdf body").unwrap();
        let BookCheck::Checked(r) = check(&path).unwrap() else {
            panic!("a pdf is checkable");
        };
        assert!(!r.complete);
        assert!(r.first_fault.unwrap().starts_with("pdf structure: "));
    }

    #[test]
    fn a_missing_file_is_a_storage_error_not_a_verdict() {
        let dir = TempDir::new().unwrap();
        for name in ["gone.epub", "gone.cbt", "gone.cb7", "gone.pdf", "gone.azw3"] {
            assert!(check(&dir.path().join(name)).is_err(), "{name}");
        }
    }
}
