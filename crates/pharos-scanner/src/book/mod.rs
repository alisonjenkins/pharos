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
    /// (`.mobi`, `.azw3`). Indexed and downloadable, never presented as
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
    /// `.mobi` / `.azw3` — no client ships a reader.
    FormatUnreadable,
    /// The archive or document is malformed, truncated, or not the format its
    /// extension claims.
    MalformedContainer,
    /// The file could not be opened at all (permissions, a dead mount).
    Unopenable,
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

/// Book extensions pharos WALKS. Distinct from
/// [`BookFormat::readable_by_client`], which answers whether a client can open
/// one — indexing a file and being able to read it are different facts, and
/// conflating them is what would put an open button on a `.mobi`.
pub const BOOK_EXTENSIONS: &[&str] = &["epub", "pdf", "cbz", "cbr", "cbt", "cb7", "mobi", "azw3"];

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
        "mobi" | "azw3" => Some(BookFormat::Unreadable),
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

    // Per-format readers land in the story that needs them (epub → US1,
    // comic → US2, pdf → US4). Until then every readable format reports itself
    // honestly as parsed-but-cover-less rather than claiming a cover.
    record_classify(
        format,
        ClassifyVerdict::CoverAbsent,
        ClassifyReason::NoCoverEntry,
    );
    Some(BookMeta {
        format,
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Metric labels are a dashboard contract: a renamed one breaks an alert
    /// silently. These exact strings appear in the quickstart's PromQL, so they
    /// are pinned here rather than left to whatever the enum happens to emit.
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
        ];

        let v: std::collections::HashSet<_> = verdicts.iter().map(|v| v.label()).collect();
        assert_eq!(v.len(), verdicts.len(), "verdict labels must be distinct");
        let r: std::collections::HashSet<_> = reasons.iter().map(|r| r.label()).collect();
        assert_eq!(r.len(), reasons.len(), "reason labels must be distinct");

        // Pinned spellings — quickstart.md §8 queries these by name.
        assert_eq!(ClassifyVerdict::CoverFound.label(), "cover_found");
        assert_eq!(ClassifyVerdict::CoverAbsent.label(), "cover_absent");
        assert_eq!(ClassifyReason::RarUnsupported.label(), "rar_unsupported");
        assert_eq!(
            ClassifyReason::UnsupportedImageEncoding.label(),
            "unsupported_image_encoding"
        );

        // Bounded cardinality: 4 formats × 4 verdicts × 7 reasons is the
        // absolute ceiling, and the reachable set is far smaller. No label
        // carries a path, a filename or an error message.
        assert!(v.len() * r.len() <= 28);
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
        for ext in ["mobi", "azw3"] {
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
