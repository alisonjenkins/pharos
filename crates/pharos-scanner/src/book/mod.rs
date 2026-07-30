//! 004-books — read book metadata out of a file, and say what it decided.
//!
//! # Why the counter exists before the branch does
//!
//! Constitution III requires that a branch choosing between behaviours record
//! its inputs, its verdict AND the reason. The scan's media-vs-book split is
//! exactly such a branch, and the first draft of this plan argued observability
//! was thin here because a book that will not open fails visibly.
//!
//! That argument covers the symptom and not the decision. "This file was
//! classified `Unreadable` because its extension is `.azw3`" and "this file
//! yielded no cover because page one was FlateDecode" are invisible from
//! outside — a user sees a book with no cover and cannot tell a bug from a
//! design limit. Two success criteria depend on being able to ask:
//!
//! * SC-003 (≥95% of covers present) is a RATE, and rates come from counters.
//! * SC-005 (what each file was classified as) is the `format` label.
//!
//! Labels are a dashboard contract: bounded cardinality, stable strings from a
//! `label()` method, asserted distinct in a test. `reason` carries a bounded
//! enumerated cause and never a free-form message — the offending VALUE goes in
//! the log line beside the counter, where cardinality does not matter.

pub mod comic;
pub mod epub;
pub mod kindle;
pub mod pdf;

use std::path::Path;

use pharos_core::{BookFormat, BookMeta};

/// What the classifier decided about a file.
///
/// The full set is declared up front because the label test asserts the whole
/// metric contract at once — a dashboard query cannot be written against
/// variants that appear one story at a time. `CoverFound` is constructed by the
/// cover extractors (T061/T062/T073) and `Unparseable` by the epub reader
/// (T047).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassifyVerdict {
    /// Metadata was read and a cover was extracted.
    CoverFound,
    /// Metadata was read; no cover came out of the file.
    CoverAbsent,
    /// The file is a recognised book extension no client can open
    /// (`.mobi`, `.azw`, `.azw3`). Indexed and downloadable, never presented as
    /// readable.
    Unreadable,
    /// The extension is a book format but the file could not be opened or
    /// parsed at all. The item still imports (V6) — with empty metadata rather
    /// than being silently dropped.
    Unparseable,
}

impl ClassifyVerdict {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ClassifyVerdict::CoverFound => "cover_found",
            ClassifyVerdict::CoverAbsent => "cover_absent",
            ClassifyVerdict::Unreadable => "unreadable",
            ClassifyVerdict::Unparseable => "unparseable",
        }
    }
}

/// WHY the verdict came out that way. Bounded — every variant is a class of
/// cause the code can distinguish, not a message.
///
/// Declared complete for the same reason as [`ClassifyVerdict`]. Constructed by:
/// `Ok` + `NoCoverEntry` by the cover extractors, `UnsupportedImageEncoding` by
/// the PDF reader (T073), `RarUnsupported` by the comic reader (T056),
/// `MalformedContainer` + `Unopenable` by the epub reader (T047).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassifyReason {
    /// Nothing to explain: a cover was found.
    Ok,
    /// The container was read but declared no cover image.
    NoCoverEntry,
    /// A cover exists but is not pass-through encodable, so extracting it would
    /// need a raster decoder this workspace deliberately does not have (R11).
    UnsupportedImageEncoding,
    /// `.cbr`. Readable by the client (libarchive.js unpacks rar) but pharos
    /// cannot extract its cover: `unrar` wraps a C library, the same objection
    /// that rules out a PDF rasteriser (R7). Permanent and by design.
    RarUnsupported,
    /// `.mobi` / `.azw` / `.azw3` — no client ships a reader.
    FormatUnreadable,
    /// The archive or document is malformed, truncated, or not the format its
    /// extension claims.
    MalformedContainer,
    /// The file could not be opened at all (permissions, a dead mount).
    Unopenable,
    /// A parser PANICKED on this file.
    ///
    /// Distinct from `MalformedContainer`, which is a parser reporting a bad
    /// file: this is a parser failing to report anything. Every book format
    /// here is parsed by a third-party crate over input a user dropped in a
    /// folder, and none of them can promise never to panic — lopdf alone holds
    /// 173 `unwrap()`s. A panic that escapes kills the whole scan RUN, so one
    /// hostile file would stop every later file being indexed. Caught, it is
    /// one skipped book with a name in the log.
    ParserPanic,
}

impl ClassifyReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ClassifyReason::Ok => "ok",
            ClassifyReason::NoCoverEntry => "no_cover_entry",
            ClassifyReason::UnsupportedImageEncoding => "unsupported_image_encoding",
            ClassifyReason::RarUnsupported => "rar_unsupported",
            ClassifyReason::FormatUnreadable => "format_unreadable",
            ClassifyReason::MalformedContainer => "malformed_container",
            ClassifyReason::Unopenable => "unopenable",
            ClassifyReason::ParserPanic => "parser_panic",
        }
    }
}

/// Run a book parser so that a PANIC inside it becomes a skipped file rather
/// than a dead scan.
///
/// V6 says a file that cannot be read is logged and skipped and the scan
/// continues. Every parser reached from here is a third-party crate handling
/// input a user dropped in a media folder, and none of them can promise never
/// to panic. Without this, one file that trips an `unwrap` deep in a decoder
/// aborts the scan TASK, and every file after it goes unindexed — a large
/// invisible failure caused by a small visible one.
///
/// This catches panics, which is the risk that remains after choosing parsers
/// with no `unsafe` and bounded recursion. It does NOT catch an abort or a
/// stack overflow; nothing in-process can, which is why the depth bound in the
/// parser itself (RUSTSEC-2026-0187, B173) mattered rather than being papered
/// over here.
pub(crate) fn guard_parser<T>(
    path: &Path,
    format: BookFormat,
    what: &'static str,
    f: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> Option<T> {
    match std::panic::catch_unwind(f) {
        Ok(v) => Some(v),
        Err(payload) => {
            // The panic message, when it is one of the two payload shapes a
            // panic actually carries. "expose the cause": a bare "it panicked"
            // sends the reader back to the file with nothing to go on.
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            record_classify(
                format,
                ClassifyVerdict::Unparseable,
                ClassifyReason::ParserPanic,
            );
            tracing::error!(
                path = %path.display(),
                stage = what,
                panic = %detail,
                "book parser PANICKED; skipping this file so the scan continues (V6)"
            );
            None
        }
    }
}

/// Record one classification decision.
///
/// Called on EVERY book path, success or failure — symmetry is the point. A
/// success path that records twelve fields beside a failure path that records
/// nothing is the shape that hid the browser-playback outage.
pub(crate) fn record_classify(
    format: BookFormat,
    verdict: ClassifyVerdict,
    reason: ClassifyReason,
) {
    metrics::counter!(
        "pharos_book_classify_total",
        "format" => format.as_str(),
        "verdict" => verdict.label(),
        "reason" => reason.label(),
    )
    .increment(1);
}

/// Re-insert the character an XML entity or char-ref stands for, so an
/// element's text reassembles intact.
///
/// quick-xml 0.41 delivers `&amp;` as its own `GeneralRef` event that SPLITS
/// the surrounding literal run, and `resolve_char_ref` answers only for the
/// NUMERIC form (`&#38;`) — a named entity comes back `Ok(None)`. Handling only
/// the numeric case silently deletes the `&` from every summary and title that
/// contains one, which is what the comic tests caught. Unknown named entities
/// are dropped rather than fatal (V6 tolerance). Same handling as the Kodi NFO
/// reader, shared here so the two book parsers cannot drift from it.
pub(crate) fn push_entity(r: &quick_xml::events::BytesRef<'_>, out: &mut String) {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        out.push(c);
    } else if let Ok(name) = r.decode() {
        match name.as_ref() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {}
        }
    }
}

/// Book extensions pharos WALKS. Distinct from
/// [`BookFormat::readable_by_client`], which answers whether a client can open
/// one — indexing a file and being able to read it are different facts, and
/// conflating them is what would put an open button on a `.mobi`.
pub const BOOK_EXTENSIONS: &[&str] = &[
    "epub", "pdf", "cbz", "cbr", "cbt", "cb7", "mobi", "azw", "azw3",
];

/// The [`BookFormat`] for a path's extension, or `None` if it is not a book at
/// all. Case-insensitive: a `.EPUB` is still an epub to pharos, even though
/// jellyfin-web's `bookPlayer` compares `Path.endsWith("epub")`
/// case-sensitively and will decline to open it (recorded in the spec under
/// Known client limitations — pharos does not misreport a path to work around
/// a client-side compare).
pub fn format_for_path(path: &Path) -> Option<BookFormat> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)?;
    match ext.as_str() {
        "epub" => Some(BookFormat::Epub),
        "pdf" => Some(BookFormat::Pdf),
        "cbz" | "cbr" | "cbt" | "cb7" => Some(BookFormat::Comic),
        // `.azw` is the older Kindle format, alongside `.azw3` and `.mobi`.
        // Present in the deployed library (30 files) and omitted from the
        // first draft of this list, which would have left them unindexed —
        // not listed, not downloadable, absent rather than merely unreadable.
        "mobi" | "azw" | "azw3" => Some(BookFormat::Unreadable),
        _ => None,
    }
}

/// True when `path` is a book, i.e. must NOT be handed to the prober.
pub fn is_book_path(path: &Path) -> bool {
    format_for_path(path).is_some()
}

/// Read what can be read from a book file.
///
/// Returns `None` only when the path is not a book at all. A book that cannot
/// be parsed still yields a `BookMeta` carrying its format, because V6 says a
/// failure must not silently drop the item — an unreadable epub should appear in
/// the library with a title from its filename, not vanish.
pub fn read_book_meta(path: &Path) -> Option<BookMeta> {
    let format = format_for_path(path)?;

    if format == BookFormat::Unreadable {
        // 005-kindle-conversion — "unreadable" is now a verdict this branch
        // REACHES rather than assumes. A `.mobi`/`.azw`/`.azw3` that carries no
        // DRM converts to an epub the client can open, so the format it is
        // readable AS is epub, and that is what gets stored. Only DRM and
        // genuine corruption still land on `Unreadable`.
        if let Some(meta) = try_kindle_meta(path) {
            return Some(meta);
        }
        record_classify(
            format,
            ClassifyVerdict::Unreadable,
            ClassifyReason::FormatUnreadable,
        );
        tracing::debug!(
            path = %path.display(),
            "book: indexed but no client can read this format"
        );
        return Some(BookMeta {
            format,
            ..Default::default()
        });
    }

    // Every arm runs behind `guard_parser`: a panic in a third-party parser
    // becomes this ONE file skipped, not the rest of the library unindexed.
    let parsed = match format {
        // Each reader records its own classify verdict, because only it knows
        // WHY (a missing cover entry, a malformed container, an unsupported
        // image encoding). A caller-side record here would flatten all of those
        // into one uninformative label.
        BookFormat::Epub => guard_parser(path, format, "epub metadata", || {
            epub::read_epub_or_empty(path)
        }),
        // A `.cbr` reaches here too and comes back cover-less by design, with
        // `rar_unsupported` on the counter (R7) — not routed away, because that
        // would make the design limit invisible.
        BookFormat::Comic => guard_parser(path, format, "comic metadata", || {
            comic::read_comic_or_empty(path)
        }),
        BookFormat::Pdf => guard_parser(path, format, "pdf metadata", || {
            pdf::read_pdf_or_empty(path)
        }),
        // Handled above.
        BookFormat::Unreadable => Some(BookMeta {
            format,
            ..Default::default()
        }),
    };
    // A panicked parse still IMPORTS the book, carrying its format — V6 again.
    // Returning `None` here would drop the item, which is the outcome the
    // guard exists to prevent.
    Some(parsed.unwrap_or(BookMeta {
        format,
        ..Default::default()
    }))
}

/// 005-kindle-conversion — try to read a Kindle file as a convertible book.
///
/// `Some` means the file converts, so it is stored as the format it is
/// readable AS: [`BookFormat::Epub`]. `None` means it stays unreadable, and
/// the caller records that verdict — DRM and corruption both land there,
/// distinguished on the convert counter rather than by pretending one is the
/// other.
///
/// The conversion is not performed here and its bytes are not kept: this call
/// wants the metadata and the cover, which are the parts a scan persists.
/// Delivery converts on demand and caches, so a scan does not pay to
/// materialise an EPUB nobody has asked for.
fn try_kindle_meta(path: &Path) -> Option<BookMeta> {
    if !kindle::is_kindle_path(path) {
        return None;
    }
    let ext = kindle::source_ext(path);
    // A panic inside the converter is one skipped book, not a dead scan
    // (V119). `guard_parser` records it on the classify counter; the convert
    // counter records it too, so "attempted" and "accounted for" reconcile.
    let Some(parsed) = guard_parser(path, BookFormat::Unreadable, "kindle metadata", || {
        kindle::read_kindle(path)
    }) else {
        kindle::record_convert(
            kindle::ConvertStage::Scan,
            ext,
            kindle::ConvertOutcome::Failed,
            kindle::ConvertReason::ParserPanic,
        );
        return None;
    };

    match parsed {
        Ok(meta) => {
            kindle::record_convert(
                kindle::ConvertStage::Scan,
                ext,
                kindle::ConvertOutcome::Converted,
                kindle::ConvertReason::Ok,
            );
            tracing::debug!(
                path = %path.display(),
                source = ext,
                title = meta.title.as_deref().unwrap_or("<none>"),
                "kindle: converts to epub; storing as a readable book"
            );
            // `to_book_meta` stamps `BookFormat::Epub` — the honest answer to
            // "what can a client read this as", and the fact the delivery path
            // reads back to know this item needs converting.
            Some(meta.to_book_meta())
        }
        Err(err) => {
            let (outcome, reason) = err.outcome();
            kindle::record_convert(kindle::ConvertStage::Scan, ext, outcome, reason);
            // DRM is not a defect and must not be logged as one: it is a
            // permanent property of the file, and 58 of the deployed library's
            // books have it. A warning per book per scan would be noise that
            // buries the real failures beside it.
            if outcome == kindle::ConvertOutcome::DrmProtected {
                tracing::debug!(
                    path = %path.display(),
                    source = ext,
                    "kindle: DRM-protected; stays unreadable by design"
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    source = ext,
                    error = %err,
                    "kindle: conversion failed; stays unreadable"
                );
            }
            None
        }
    }
}

/// Read a book's cover image bytes, and RECORD what happened.
///
/// This is the only step that can tell a cover from the promise of one, so it
/// is where the cover verdict is recorded. A manifest entry is not a cover: an
/// OPF can name an image the archive does not contain, and reporting
/// `cover_found` on the strength of the name would advertise a `Primary` tag
/// that 404s on every grid render — the B149 shape the counter exists to make
/// visible.
///
/// Returns `None` for every no-cover outcome, having said WHY on the counter
/// first. The caller writes nothing, and `has_primary_art` stays false, so the
/// item advertises no `Primary` tag.
pub fn read_book_cover(path: &Path) -> Option<Vec<u8>> {
    let format = format_for_path(path)?;
    match format {
        BookFormat::Epub => {
            match guard_parser(path, format, "epub cover", || epub::read_epub_cover(path))? {
                Ok(Some(bytes)) => {
                    record_classify(format, ClassifyVerdict::CoverFound, ClassifyReason::Ok);
                    Some(bytes)
                }
                Ok(None) => {
                    record_classify(
                        format,
                        ClassifyVerdict::CoverAbsent,
                        ClassifyReason::NoCoverEntry,
                    );
                    None
                }
                Err(err) => {
                    record_classify(
                        format,
                        ClassifyVerdict::Unparseable,
                        ClassifyReason::MalformedContainer,
                    );
                    tracing::warn!(path = %path.display(), error = %err, "epub cover unreadable");
                    None
                }
            }
        }
        BookFormat::Comic => match guard_parser(path, format, "comic cover", || {
            comic::read_comic_cover(path)
        })? {
            Ok(Some(bytes)) => {
                record_classify(format, ClassifyVerdict::CoverFound, ClassifyReason::Ok);
                Some(bytes)
            }
            Ok(None) => {
                record_classify(
                    format,
                    ClassifyVerdict::CoverAbsent,
                    ClassifyReason::NoCoverEntry,
                );
                None
            }
            Err(err) => {
                let (verdict, reason) = match &err {
                    comic::ComicError::RarUnsupported(_) => {
                        (ClassifyVerdict::CoverAbsent, ClassifyReason::RarUnsupported)
                    }
                    comic::ComicError::Open(_) => {
                        (ClassifyVerdict::Unparseable, ClassifyReason::Unopenable)
                    }
                    _ => (
                        ClassifyVerdict::Unparseable,
                        ClassifyReason::MalformedContainer,
                    ),
                };
                record_classify(format, verdict, reason);
                if matches!(err, comic::ComicError::RarUnsupported(_)) {
                    tracing::debug!(path = %path.display(), reason = %err, "comic: no cover by design");
                } else {
                    tracing::warn!(path = %path.display(), error = %err, "comic cover unreadable");
                }
                None
            }
        },
        // Pass-through only: page one's embedded image when it is already a
        // JPEG. No rasterisation, permanently (R11) — so "an image pharos
        // cannot extract" and "no image at all" get DIFFERENT reasons, because
        // they have different fixes and only one of them is a design limit.
        BookFormat::Pdf => {
            match guard_parser(path, format, "pdf cover", || pdf::read_pdf_cover(path))? {
                Ok(pdf::PdfCover::Jpeg(bytes)) => {
                    record_classify(format, ClassifyVerdict::CoverFound, ClassifyReason::Ok);
                    Some(bytes)
                }
                Ok(pdf::PdfCover::UnsupportedEncoding(filter)) => {
                    record_classify(
                        format,
                        ClassifyVerdict::CoverAbsent,
                        ClassifyReason::UnsupportedImageEncoding,
                    );
                    tracing::debug!(
                        path = %path.display(),
                        filter = %filter,
                        "pdf: page-one image is not pass-through encodable; no cover by design (R11)"
                    );
                    None
                }
                Ok(pdf::PdfCover::NoImage) => {
                    record_classify(
                        format,
                        ClassifyVerdict::CoverAbsent,
                        ClassifyReason::NoCoverEntry,
                    );
                    None
                }
                Err(err) => {
                    record_classify(
                        format,
                        ClassifyVerdict::Unparseable,
                        ClassifyReason::MalformedContainer,
                    );
                    tracing::warn!(path = %path.display(), error = %err, "pdf cover unreadable");
                    None
                }
            }
        }
        // 005-kindle-conversion — a Kindle file gets a real attempt here.
        // Every one of the 17 convertible files in the deployed library
        // declares a cover, and before conversion they could only get one from
        // a `cover.jpg` sidecar (31%).
        //
        // The verdict is recorded against `Epub`, matching what
        // `read_book_meta` stored: SC-003 is a rate over readable books, and
        // filing a converted book's cover under a format no client can read
        // would put it outside the population the rate measures.
        BookFormat::Unreadable if kindle::is_kindle_path(path) => {
            match guard_parser(path, format, "kindle cover", || {
                kindle::read_kindle_cover(path)
            })? {
                Ok(Some(bytes)) => {
                    record_classify(
                        BookFormat::Epub,
                        ClassifyVerdict::CoverFound,
                        ClassifyReason::Ok,
                    );
                    Some(bytes)
                }
                Ok(None) => {
                    record_classify(
                        BookFormat::Epub,
                        ClassifyVerdict::CoverAbsent,
                        ClassifyReason::NoCoverEntry,
                    );
                    None
                }
                // DRM and corruption both land here, and they are NOT the same
                // fact — one is permanent and expected, the other is a file
                // worth looking at. `read_book_meta` already recorded this
                // file's convert outcome, so nothing is counted twice; only
                // the cover verdict is added.
                Err(err) => {
                    let drm = matches!(err, kindle::KindleError::Drm(_));
                    record_classify(
                        format,
                        if drm {
                            ClassifyVerdict::Unreadable
                        } else {
                            ClassifyVerdict::Unparseable
                        },
                        if drm {
                            ClassifyReason::FormatUnreadable
                        } else {
                            ClassifyReason::MalformedContainer
                        },
                    );
                    if !drm {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "kindle cover unreadable"
                        );
                    }
                    None
                }
            }
        }
        // Already recorded as `format_unreadable` by `read_book_meta`; a second
        // verdict for the same file would double-count the rate SC-003 reads.
        BookFormat::Unreadable => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Metric labels are a dashboard contract: a renamed one breaks an alert
    /// silently. These exact strings appear in the quickstart's PromQL, so they
    /// are pinned here rather than left to whatever the enum happens to emit.
    /// NOTE (V118 applies to this test too): the arrays below are enumerated by
    /// HAND, and Rust cannot check them for completeness without a derive. The
    /// compiler does force `label()` to gain an arm for a new variant, so a new
    /// variant cannot be label-less — but it CAN be absent from here and go
    /// unasserted. Adding a variant means adding it in both places.
    #[test]
    fn classify_labels_are_distinct_and_stable() {
        let verdicts = [
            ClassifyVerdict::CoverFound,
            ClassifyVerdict::CoverAbsent,
            ClassifyVerdict::Unreadable,
            ClassifyVerdict::Unparseable,
        ];
        let reasons = [
            ClassifyReason::Ok,
            ClassifyReason::NoCoverEntry,
            ClassifyReason::UnsupportedImageEncoding,
            ClassifyReason::RarUnsupported,
            ClassifyReason::FormatUnreadable,
            ClassifyReason::MalformedContainer,
            ClassifyReason::Unopenable,
            ClassifyReason::ParserPanic,
        ];

        let v: std::collections::HashSet<_> = verdicts.iter().map(|v| v.label()).collect();
        assert_eq!(v.len(), verdicts.len(), "verdict labels must be distinct");
        let r: std::collections::HashSet<_> = reasons.iter().map(|r| r.label()).collect();
        assert_eq!(r.len(), reasons.len(), "reason labels must be distinct");

        // Pinned spellings — quickstart.md §8 queries these by name.
        assert_eq!(ClassifyVerdict::CoverFound.label(), "cover_found");
        assert_eq!(ClassifyVerdict::CoverAbsent.label(), "cover_absent");
        assert_eq!(ClassifyReason::RarUnsupported.label(), "rar_unsupported");
        assert_eq!(ClassifyReason::ParserPanic.label(), "parser_panic");
        assert_eq!(
            ClassifyReason::UnsupportedImageEncoding.label(),
            "unsupported_image_encoding"
        );

        // Bounded cardinality: 4 formats × 4 verdicts × 7 reasons is the
        // absolute ceiling, and the reachable set is far smaller. No label
        // carries a path, a filename or an error message.
        assert!(v.len() * r.len() <= 32);
    }

    /// B173 follow-up — a panicking parser must cost ONE file, not the scan.
    ///
    /// The risk is asymmetric: a panic that escapes `read_book_meta` kills the
    /// scan task, so every file after the bad one goes unindexed. That is a
    /// large invisible failure caused by a small visible one, and it is exactly
    /// what V6 exists to prevent.
    ///
    /// `guard_parser` is exercised directly with a panicking closure, because a
    /// test that waits for a real parser to panic is a test that passes for the
    /// wrong reason the day the parser is fixed.
    #[test]
    fn a_panicking_parser_skips_one_file_and_records_why() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // Silence the panic hook so the test output stays readable; the panic
        // is the point, not the backtrace.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out: Option<u32> = guard_parser(
            std::path::Path::new("/books/Hostile.pdf"),
            BookFormat::Pdf,
            "pdf metadata",
            || panic!("called `Option::unwrap()` on a `None` value"),
        );
        std::panic::set_hook(prev);

        assert_eq!(out, None, "the guard must swallow the panic, not resume it");

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
            panic!(
                "a panicking parser must be VISIBLE — silently skipping a file is the \
                    failure shape this whole counter exists to prevent"
            )
        };
        assert_eq!(labels.get("format").map(String::as_str), Some("pdf"));
        assert_eq!(
            labels.get("verdict").map(String::as_str),
            Some("unparseable")
        );
        assert_eq!(
            labels.get("reason").map(String::as_str),
            Some("parser_panic"),
            "a panic is NOT malformed_container: one is the parser reporting a bad \
             file, the other is the parser failing to report at all"
        );
    }

    /// …and the item still IMPORTS. Dropping it would trade a dead scan for a
    /// vanished book, which V6 forbids just as firmly.
    #[test]
    fn a_book_whose_parser_panics_still_imports() {
        let td = tempfile::tempdir().unwrap();
        // Not a PDF at all; the real reader rejects it cleanly rather than
        // panicking, which is the honest state of the world — what is asserted
        // is the CONTRACT that a failed parse still yields a row.
        let p = td.path().join("Hostile.pdf");
        std::fs::write(&p, b"%PDF-1.4\ngarbage").unwrap();
        let meta = read_book_meta(&p).expect("a book must still import");
        assert_eq!(meta.format, BookFormat::Pdf);
    }

    #[test]
    fn every_book_extension_maps_to_a_format() {
        for ext in BOOK_EXTENSIONS {
            let p = std::path::PathBuf::from(format!("/books/x.{ext}"));
            assert!(
                format_for_path(&p).is_some(),
                "{ext} is walked but maps to no BookFormat"
            );
            assert!(is_book_path(&p), "{ext} must never reach the prober");
        }
        // A media file is not a book, or the scan would stop probing videos.
        assert_eq!(format_for_path(std::path::Path::new("/m/a.mkv")), None);
        assert!(!is_book_path(std::path::Path::new("/m/a.mkv")));
    }

    /// Walking a file and being able to read it are DIFFERENT questions, and
    /// `BookFormat::readable_by_client` is the only authority on the second.
    #[test]
    fn walking_a_book_is_not_the_same_question_as_reading_it() {
        for ext in ["mobi", "azw", "azw3"] {
            let p = std::path::PathBuf::from(format!("/books/x.{ext}"));
            let f = format_for_path(&p).expect("indexed");
            assert!(BOOK_EXTENSIONS.contains(&ext), "{ext} must be walked");
            assert!(
                !f.readable_by_client(),
                "{ext} has no client reader, so it must not claim to be readable"
            );
        }
        for ext in ["epub", "pdf", "cbz", "cbr", "cbt", "cb7"] {
            let p = std::path::PathBuf::from(format!("/books/x.{ext}"));
            let f = format_for_path(&p).expect("indexed");
            assert!(
                f.readable_by_client(),
                "{ext} has a client reader and must say so"
            );
        }
    }

    /// A `.EPUB` is still a book to pharos. The client's case-sensitive compare
    /// is the client's business; misreporting the path to work around it is what
    /// the spec forbids.
    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(
            format_for_path(std::path::Path::new("/b/X.EPUB")),
            Some(BookFormat::Epub)
        );
        assert_eq!(
            format_for_path(std::path::Path::new("/b/X.CbZ")),
            Some(BookFormat::Comic)
        );
    }

    #[test]
    fn an_unreadable_format_is_recorded_as_such() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let meta = read_book_meta(std::path::Path::new("/books/Dune.azw3")).expect("still imports");
        assert_eq!(
            meta.format,
            BookFormat::Unreadable,
            "V6 — the item imports even though no client can open it"
        );

        let found = snapshotter
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
        let Some(labels) = found else {
            panic!("no pharos_book_classify_total series recorded — the decision is invisible");
        };
        assert_eq!(labels.get("format").map(String::as_str), Some("unreadable"));
        assert_eq!(
            labels.get("verdict").map(String::as_str),
            Some("unreadable")
        );
        assert_eq!(
            labels.get("reason").map(String::as_str),
            Some("format_unreadable"),
            "the reason must name WHY, not just that it failed"
        );
    }
}
