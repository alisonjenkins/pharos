//! PDF reading (004-books, US4).
//!
//! Two questions, same as every other book format: how many pages, and what
//! does the file say it is. `lopdf` answers both from the page tree and the
//! document info dictionary without rendering a thing.
//!
//! # Covers: pass-through only, and that is permanent (R11)
//!
//! A PDF's first page is usually a drawing program, not an image — text
//! operators, fonts, vector paths. Turning that into a cover means
//! RASTERISING, and `lopdf` cannot rasterise. Neither can anything else in this
//! workspace: there is no pure-Rust image decoder in the tree, and adding
//! either a rasteriser or a decoder means a C library, which breaks the
//! single-binary deploy that also rules out a rar reader (R7).
//!
//! So exactly one case yields a cover: page one's content is a single embedded
//! image whose filter is `DCTDecode`. That filter IS baseline JPEG — the stream
//! bytes are a complete JPEG file, so they go to the image cache verbatim with
//! no decode step at all. Scanned books and comics-as-PDF hit this; a
//! text-first PDF does not, and records `unsupported_image_encoding` so the gap
//! is a number rather than a mystery.
//!
//! This is NARROWER than FR-006 reads at first glance. It is asserted in a test
//! so it cannot be quietly widened into something that needs a decoder.

use std::path::Path;

use pharos_core::{BookFormat, BookMeta};

use super::{record_classify, ClassifyReason, ClassifyVerdict};

/// Everything the PDF told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfMetadata {
    /// Page count from the page tree — the document's own structure, not a
    /// metadata field a producer may have left stale.
    pub page_count: Option<u32>,
    pub title: Option<String>,
    pub author: Option<String>,
    /// `/Producer` — the closest thing a PDF has to a publisher, and usually
    /// the software that made it. Kept because it is occasionally a real
    /// imprint; the provider decides whether to surface it.
    pub producer: Option<String>,
    /// `/Subject`, the info dictionary's blurb field.
    pub description: Option<String>,
    /// `/CreationDate`, verbatim (`D:19650801000000Z` and friends).
    pub date: Option<String>,
}

impl PdfMetadata {
    pub fn to_book_meta(&self) -> BookMeta {
        BookMeta {
            format: BookFormat::Pdf,
            // A PDF page IS a page: fixed, viewport-independent, unlike an
            // epub's reflowed pagination.
            page_count: self.page_count,
            author: self.author.clone(),
            publisher: self.producer.clone(),
            series_name: None,
            series_index: None,
            isbn: None,
        }
    }
}

/// Why a PDF could not be read. Carries the offending value in every case.
#[derive(Debug)]
pub enum PdfError {
    Open { path: String, detail: String },
    Malformed(String),
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PdfError::Open { path, detail } => write!(f, "cannot open {path:?}: {detail}"),
            PdfError::Malformed(detail) => write!(f, "not a readable PDF: {detail}"),
        }
    }
}

/// Read a PDF's page count and info dictionary.
pub fn read_pdf(path: &Path) -> Result<PdfMetadata, PdfError> {
    let doc = load(path)?;
    let mut out = PdfMetadata {
        page_count: Some(doc.get_pages().len() as u32),
        ..Default::default()
    };

    // `/Info` is optional and frequently absent — a PDF with no info dictionary
    // is a normal PDF, not a broken one, so this is not an error path.
    if let Ok(info) = doc
        .trailer
        .get(b"Info")
        .and_then(|o| doc.get_object(o.as_reference()?))
        .and_then(|o| o.as_dict())
    {
        out.title = text_field(&doc, info, b"Title");
        out.author = text_field(&doc, info, b"Author");
        out.producer = text_field(&doc, info, b"Producer");
        out.description = text_field(&doc, info, b"Subject");
        out.date = text_field(&doc, info, b"CreationDate");
    }
    Ok(out)
}

/// The scan path's decision: a parse failure yields empty metadata rather than
/// dropping the item (V6), and records WHY.
///
/// Records only the PARSE outcome; the cover verdict belongs to
/// [`read_pdf_cover`], the only step that knows whether bytes came out.
pub fn read_pdf_or_empty(path: &Path) -> BookMeta {
    match read_pdf(path) {
        Ok(meta) => meta.to_book_meta(),
        Err(err) => {
            let (verdict, reason) = match &err {
                PdfError::Open { .. } => (ClassifyVerdict::Unparseable, ClassifyReason::Unopenable),
                PdfError::Malformed(_) => (
                    ClassifyVerdict::Unparseable,
                    ClassifyReason::MalformedContainer,
                ),
            };
            record_classify(BookFormat::Pdf, verdict, reason);
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "PDF unreadable; importing with filename title only"
            );
            BookMeta {
                format: BookFormat::Pdf,
                ..Default::default()
            }
        }
    }
}

/// The outcome of looking for a PDF cover.
///
/// Three states, not two: "no image at all" and "an image pharos cannot pass
/// through" are different facts with different reasons on the counter, and
/// collapsing them would make the R11 narrowing invisible.
#[derive(Debug)]
pub enum PdfCover {
    /// Page one is a single `DCTDecode` image: these bytes ARE a JPEG.
    Jpeg(Vec<u8>),
    /// Page one carries an image, but in an encoding that would need a decoder
    /// this workspace deliberately does not have. Names the filter.
    UnsupportedEncoding(String),
    /// Page one carries no image at all — a text-first page.
    NoImage,
}

/// Extract page one's embedded JPEG, if that is what page one is.
///
/// No rasterisation, by design (R11). See the module header.
pub fn read_pdf_cover(path: &Path) -> Result<PdfCover, PdfError> {
    let doc = load(path)?;
    let Some((_, page_id)) = doc.get_pages().into_iter().next() else {
        return Ok(PdfCover::NoImage);
    };

    // Resources may be inherited from an ancestor Pages node rather than
    // carried on the page itself — a perfectly ordinary layout that a
    // page-dict-only lookup silently reports as image-less.
    let Some(resources) = resources_for(&doc, page_id) else {
        return Ok(PdfCover::NoImage);
    };
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|o| resolve_dict(&doc, o))
    else {
        return Ok(PdfCover::NoImage);
    };

    let mut unsupported: Option<String> = None;
    for (_, obj) in xobjects.iter() {
        let Some(stream) = resolve_stream(&doc, obj) else {
            continue;
        };
        let is_image = stream
            .dict
            .get(b"Subtype")
            .and_then(|s| s.as_name())
            .map(|n| n == b"Image")
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        match filter_name(&stream.dict) {
            // The stream bytes are a complete baseline JPEG. Straight through.
            Some(f) if f == "DCTDecode" => return Ok(PdfCover::Jpeg(stream.content.clone())),
            // Named so the log says WHICH encoding, not merely that one was
            // unsupported. Keep looking — a page can hold several XObjects and
            // a later one may be the JPEG.
            Some(f) => unsupported = Some(f),
            None => unsupported = Some("(no filter)".into()),
        }
    }

    Ok(match unsupported {
        Some(f) => PdfCover::UnsupportedEncoding(f),
        None => PdfCover::NoImage,
    })
}

fn load(path: &Path) -> Result<lopdf::Document, PdfError> {
    if !path.exists() {
        return Err(PdfError::Open {
            path: path.display().to_string(),
            detail: "no such file".into(),
        });
    }
    lopdf::Document::load(path).map_err(|e| PdfError::Malformed(e.to_string()))
}

/// A page's `/Resources`, following the inheritance chain up the page tree.
///
/// Bounded to 32 hops: a malformed PDF can make `/Parent` a cycle, and this
/// runs over whatever a user drops in a media folder.
fn resources_for(doc: &lopdf::Document, page_id: lopdf::ObjectId) -> Option<&lopdf::Dictionary> {
    let mut node = doc.get_dictionary(page_id).ok()?;
    for _ in 0..32 {
        if let Some(res) = node
            .get(b"Resources")
            .ok()
            .and_then(|o| resolve_dict(doc, o))
        {
            return Some(res);
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = doc.get_dictionary(parent).ok()?;
    }
    None
}

/// A dictionary, whether it is inline or behind a reference.
fn resolve_dict<'a>(
    doc: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Dictionary> {
    match obj {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok()?.as_dict().ok(),
        other => other.as_dict().ok(),
    }
}

/// A stream, whether it is inline or behind a reference.
fn resolve_stream<'a>(
    doc: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Stream> {
    match obj {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok()?.as_stream().ok(),
        other => other.as_stream().ok(),
    }
}

/// The stream's filter name. `/Filter` may be a single name or an array of
/// them; an array means chained encodings, and only a lone `DCTDecode` is
/// pass-through — so an array reports its LAST entry, which is the outermost
/// encoding a consumer would have to undo first.
fn filter_name(dict: &lopdf::Dictionary) -> Option<String> {
    let f = dict.get(b"Filter").ok()?;
    match f {
        lopdf::Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        lopdf::Object::Array(a) => {
            let names: Vec<String> = a
                .iter()
                .filter_map(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            match names.len() {
                0 => None,
                // A single-element array is the same thing as a bare name.
                1 => names.into_iter().next(),
                _ => Some(names.join("+")),
            }
        }
        _ => None,
    }
}

/// A PDF text-string info field, decoded.
///
/// Three encodings, in the order they can be told apart:
///
/// 1. **UTF-16BE with a `FE FF` byte-order mark** — the spec's way to carry
///    anything outside PDFDocEncoding.
/// 2. **UTF-8** — not legal in a PDF text string, and written anyway by enough
///    producers to matter. Tried before the fallback because it is
///    self-validating: a byte run that decodes as UTF-8 with multi-byte
///    sequences is almost never the Latin-1 text those same bytes would spell.
/// 3. **PDFDocEncoding** (near-Latin-1), the default.
///
/// Getting this wrong turns a perfectly good title into mojibake, which reads
/// as bad metadata rather than as a decode bug — nobody files that.
fn text_field(doc: &lopdf::Document, dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
    let obj = dict.get(key).ok()?;
    let bytes = match obj {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok()?.as_str().ok()?,
        other => other.as_str().ok()?,
    };
    let text = if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|b| *b as char).collect(),
        }
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};

    /// Bytes that are a complete baseline JPEG. A `DCTDecode` stream's content
    /// IS this — which is the whole reason the pass-through works.
    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// Build a PDF in-test rather than checking a binary blob into the repo, so
    /// the fixture's contents are visible in the test that reads them.
    ///
    /// `image` is `Some((filter, bytes))` to give page one an XObject.
    fn write_pdf(path: &Path, with_info: bool, image: Option<(&str, Vec<u8>)>) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let mut page_dict = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
        };
        if let Some((filter, bytes)) = image {
            let mut stream = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => 1,
                    "Height" => 1,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => Object::Name(filter.as_bytes().to_vec()),
                },
                bytes,
            );
            // lopdf would otherwise re-compress on save and overwrite `Filter`,
            // which is the exact field under test.
            stream.allows_compression = false;
            let img_id = doc.add_object(stream);
            page_dict.set(
                "Resources",
                dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
            );
        }
        let page_id = doc.add_object(page_dict);

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        if with_info {
            let info_id = doc.add_object(dictionary! {
                "Title" => Object::string_literal("Gödel, Escher, Bach"),
                "Author" => Object::string_literal("Douglas Hofstadter"),
                "Producer" => Object::string_literal("Basic Books"),
                "Subject" => Object::string_literal("An Eternal Golden Braid."),
                "CreationDate" => Object::string_literal("D:19790101000000Z"),
            });
            doc.trailer.set("Info", info_id);
        }
        doc.save(path).unwrap();
    }

    /// T071.
    #[test]
    fn a_pdfs_info_dictionary_and_page_count_are_read() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("GEB.pdf");
        write_pdf(&p, true, None);

        let meta = read_pdf(&p).expect("a well-formed PDF must parse");
        assert_eq!(
            meta.page_count,
            Some(1),
            "the page count comes from the page tree, which is the document's \
             own structure"
        );
        assert_eq!(meta.title.as_deref(), Some("Gödel, Escher, Bach"));
        assert_eq!(meta.author.as_deref(), Some("Douglas Hofstadter"));
        assert_eq!(meta.producer.as_deref(), Some("Basic Books"));
        assert_eq!(
            meta.description.as_deref(),
            Some("An Eternal Golden Braid.")
        );
        assert_eq!(meta.date.as_deref(), Some("D:19790101000000Z"));

        let bm = meta.to_book_meta();
        assert_eq!(bm.format, BookFormat::Pdf);
        assert_eq!(bm.page_count, Some(1));
        assert_eq!(bm.author.as_deref(), Some("Douglas Hofstadter"));
    }

    /// An info dictionary is optional. A PDF without one is a normal PDF.
    #[test]
    fn a_pdf_without_an_info_dictionary_still_reports_its_pages() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Bare.pdf");
        write_pdf(&p, false, None);

        let meta = read_pdf(&p).expect("no /Info is not a parse failure");
        assert_eq!(meta.page_count, Some(1));
        assert_eq!(meta.title, None);
        assert_eq!(meta.author, None);
    }

    /// T072 — the R11 narrowing, asserted so it cannot be quietly widened into
    /// something that needs a rasteriser or an image decoder.
    #[test]
    fn a_pdf_cover_comes_out_only_when_page_one_is_already_a_jpeg() {
        let td = tempfile::tempdir().unwrap();

        // Scanned book / comic-as-PDF: page one IS a JPEG.
        let scanned = td.path().join("Scanned.pdf");
        write_pdf(&scanned, false, Some(("DCTDecode", jpeg())));
        let cover = read_pdf_cover(&scanned).unwrap();
        let PdfCover::Jpeg(bytes) = cover else {
            panic!("a DCTDecode page-one image must pass straight through, got {cover:?}");
        };
        assert_eq!(
            bytes,
            jpeg(),
            "pass-through means BYTE-for-byte: there is no decode step, and a \
             re-encode here would mean a decoder in the tree"
        );
        assert_eq!(
            &bytes[..2],
            &[0xFF, 0xD8],
            "and what comes out must actually start with a JPEG SOI marker"
        );

        // The same page, FlateDecode. Extracting this needs a zlib inflate AND
        // then a raster encode — a decoder, which is what R11 rules out.
        let flate = td.path().join("Flate.pdf");
        write_pdf(&flate, false, Some(("FlateDecode", vec![0u8; 64])));
        let cover = read_pdf_cover(&flate).unwrap();
        let PdfCover::UnsupportedEncoding(filter) = cover else {
            panic!("a FlateDecode image is NOT extractable without a decoder, got {cover:?}");
        };
        assert_eq!(
            filter, "FlateDecode",
            "the reason must name the encoding, not just say unsupported"
        );

        // A text-first PDF: no image at all. Distinct from the case above —
        // "nothing to extract" and "something pharos cannot extract" have
        // different fixes and so must have different reasons.
        let text = td.path().join("Text.pdf");
        write_pdf(&text, true, None);
        assert!(
            matches!(read_pdf_cover(&text).unwrap(), PdfCover::NoImage),
            "a text-first page carries no image"
        );
    }

    /// The counter must distinguish the three outcomes, because SC-003's rate
    /// is meaningless if "no cover" is one undifferentiated bucket.
    #[test]
    fn each_cover_outcome_records_its_own_reason() {
        use metrics_util::debugging::DebuggingRecorder;

        let td = tempfile::tempdir().unwrap();
        /// (fixture name, optional page-one image, expected verdict, expected reason)
        type Case = (
            &'static str,
            Option<(&'static str, Vec<u8>)>,
            &'static str,
            &'static str,
        );
        let cases: [Case; 3] = [
            (
                "Scanned.pdf",
                Some(("DCTDecode", jpeg())),
                "cover_found",
                "ok",
            ),
            (
                "Flate.pdf",
                Some(("FlateDecode", vec![0u8; 64])),
                "cover_absent",
                "unsupported_image_encoding",
            ),
            ("Text.pdf", None, "cover_absent", "no_cover_entry"),
        ];

        for (name, image, want_verdict, want_reason) in cases {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let _guard = metrics::set_default_local_recorder(&recorder);

            let p = td.path().join(name);
            write_pdf(&p, false, image);
            let _ = crate::book::read_book_cover(&p);

            let labels = snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(k, _, _, _)| {
                    let key = k.key();
                    (key.name() == "pharos_book_classify_total").then(|| {
                        key.labels()
                            .map(|l| (l.key().to_string(), l.value().to_string()))
                            .collect::<std::collections::HashMap<_, _>>()
                    })
                });
            let Some(labels) = labels else {
                panic!("{name}: the cover decision must be visible")
            };
            assert_eq!(
                labels.get("format").map(String::as_str),
                Some("pdf"),
                "{name}"
            );
            assert_eq!(
                labels.get("verdict").map(String::as_str),
                Some(want_verdict),
                "{name}"
            );
            assert_eq!(
                labels.get("reason").map(String::as_str),
                Some(want_reason),
                "{name}"
            );
        }
    }

    #[test]
    fn a_malformed_pdf_still_imports_the_item() {
        let td = tempfile::tempdir().unwrap();

        let p = td.path().join("Lying.pdf");
        std::fs::write(&p, b"this is plain text").unwrap();
        assert!(read_pdf(&p).is_err(), "precondition: this cannot parse");
        assert_eq!(
            read_pdf_or_empty(&p).format,
            BookFormat::Pdf,
            "V6 — the item still imports, carrying its format"
        );

        let p = td.path().join("Gone.pdf");
        let err = read_pdf(&p).expect_err("absent");
        assert!(
            err.to_string().contains("Gone.pdf"),
            "the error must name the file, got: {err}"
        );
    }

    /// `/Resources` is often carried on an ancestor `Pages` node rather than on
    /// the page. Looking only at the page dict reports a perfectly ordinary
    /// scanned PDF as image-less.
    #[test]
    fn resources_are_found_when_inherited_from_the_page_tree() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Inherited.pdf");

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => Object::Name(b"DCTDecode".to_vec()),
            },
            jpeg(),
        );
        stream.allows_compression = false;
        let img_id = doc.add_object(stream);
        // The page itself declares NO resources.
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&p).unwrap();

        assert!(
            matches!(read_pdf_cover(&p).unwrap(), PdfCover::Jpeg(_)),
            "an inherited /Resources must still be found, or every PDF laid out \
             this way reports no cover"
        );
    }

    /// The third encoding: real PDFDocEncoding bytes, which are NOT valid
    /// UTF-8 and so must fall through to the Latin-1 reading. Without this the
    /// UTF-8 attempt above could look like it made things strictly better while
    /// silently breaking every conforming producer.
    #[test]
    fn a_pdfdocencoding_title_decodes_by_falling_back() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Latin1.pdf");

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id });
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

        // "Café" in Latin-1: 0xE9 is a lone high byte, invalid as UTF-8.
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::String(b"Caf\xE9".to_vec(), lopdf::StringFormat::Literal),
        });
        doc.trailer.set("Info", info_id);
        doc.save(&p).unwrap();

        assert_eq!(
            read_pdf(&p).unwrap().title.as_deref(),
            Some("Café"),
            "conforming PDFDocEncoding must still decode after the UTF-8 attempt fails"
        );
    }

    #[test]
    fn a_utf16_title_decodes_rather_than_becoming_mojibake() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Utf16.pdf");

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id });
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

        // UTF-16BE with a byte-order mark, the other legal PDF text encoding.
        let mut bytes = vec![0xFE, 0xFF];
        for u in "Дюна".encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::String(bytes, lopdf::StringFormat::Literal),
        });
        doc.trailer.set("Info", info_id);
        doc.save(&p).unwrap();

        assert_eq!(
            read_pdf(&p).unwrap().title.as_deref(),
            Some("Дюна"),
            "a BOM-prefixed UTF-16BE title must decode, not arrive as bytes-as-chars"
        );
    }
}
