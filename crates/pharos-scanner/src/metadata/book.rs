//! 004-books (T066) — descriptive metadata out of the book file itself.
//!
//! An epub's OPF and a comic's `ComicInfo.xml` carry the same kind of thing a
//! movie MKV carries in its container tags: a title, a byline, a publisher, a
//! date, a blurb. So this is the [`EmbeddedTagProvider`]'s sibling and sits at
//! the same priority — a user-curated `.nfo` still wins any scalar, and a
//! filename still supplies a title when the file itself named none.
//!
//! # What lands here and what lands on `BookMeta`
//!
//! `BookMeta` holds the book-SPECIFIC facts: format, page count, ISBN, series
//! name and index. Everything a movie also has — title, overview, release date,
//! publisher — is an ordinary item field and goes through the resolver like any
//! other source (R6). Putting a release date on `BookMeta` would give the
//! library two places to look for the same fact and let them disagree.
//!
//! # Publisher is a studio
//!
//! Jellyfin has no publisher field; `Studios` is where a client displays "who
//! put this out", and it is what the existing studio join persists. So the
//! publisher goes there rather than being dropped for want of an exact match.

use pharos_core::{
    DomainResult, MediaKind, MetadataProvider, MetadataRequest, MetadataResult, PersonKind,
    PersonRef,
};

use super::nfo::parse_date_unix;

/// Merge priority — the same rung as embedded container tags: below NFO (100)
/// and sidecar (50), above filename (10).
pub const PRIORITY: i32 = 30;

/// Stateless book-file metadata provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct BookMetadataProvider;

impl BookMetadataProvider {
    pub fn new() -> Self {
        Self
    }
}

/// The four descriptive fields a book file can supply, normalised across the
/// two container shapes so the provider below has one thing to map.
#[derive(Debug, Default)]
struct BookDescriptive {
    title: Option<String>,
    author: Option<String>,
    publisher: Option<String>,
    description: Option<String>,
    /// Verbatim from the file: an epub's `dc:date` (often a full ISO
    /// timestamp, sometimes a bare year) or a comic's Year/Month/Day joined.
    date: Option<String>,
}

fn read_descriptive(path: &std::path::Path) -> Option<BookDescriptive> {
    match crate::book::format_for_path(path)? {
        pharos_core::BookFormat::Epub => {
            let m = crate::book::epub::read_epub(path).ok()?;
            Some(BookDescriptive {
                title: m.title,
                author: m.author,
                publisher: m.publisher,
                description: m.description,
                date: m.date,
            })
        }
        pharos_core::BookFormat::Comic => {
            let m = crate::book::comic::read_comic(path).ok()?;
            Some(BookDescriptive {
                title: m.title,
                author: m.author,
                publisher: m.publisher,
                description: m.description,
                date: m.date,
            })
        }
        // A PDF's info dictionary lands with US4 (T073); mobi/azw3 carry
        // nothing pharos can read. Both return nothing rather than erroring —
        // the filename provider still supplies a title (FR-007).
        pharos_core::BookFormat::Pdf | pharos_core::BookFormat::Unreadable => None,
    }
}

/// The leading 4-digit year of a date string, whatever else it carries.
///
/// `dc:date` is famously loose: `1965`, `1965-08-01`, `1965-08-01T00:00:00Z`
/// and `08/1965` all occur. A year is recoverable from all but the last, and a
/// year is what `production_year` wants; the full date only sets
/// `premiere_date`, which needs all three components.
fn leading_year(text: &str) -> Option<u32> {
    let digits: String = text
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (digits.len() == 4).then(|| digits.parse().ok())?
}

impl MetadataProvider for BookMetadataProvider {
    fn name(&self) -> &'static str {
        "book"
    }

    fn priority(&self) -> i32 {
        PRIORITY
    }

    fn supports(&self, kind: MediaKind) -> bool {
        // Only books. Reading an MKV as an epub would fail on every video in
        // the library — cheaply, but it would log a warning per file.
        matches!(kind, MediaKind::Book)
    }

    async fn fetch(&self, req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        // Archive IO on a blocking thread (V5). A read failure yields an empty
        // result rather than `Err`: the book still imports, titled from its
        // filename, and the classify counter has already recorded WHY the file
        // could not be read (V6).
        let path = req.path.to_path_buf();
        let Ok(Some(d)) = tokio::task::spawn_blocking(move || read_descriptive(&path)).await else {
            return Ok(MetadataResult::default());
        };

        let clean = |s: Option<String>| {
            s.map(|s| s.trim().to_string())
                .filter(|s: &String| !s.is_empty())
        };
        let title = clean(d.title);
        let publisher = clean(d.publisher);
        let date = clean(d.date);

        Ok(MetadataResult {
            // `None` here falls through to the filename provider, which is
            // exactly what FR-007 asks for — no book is ever listed untitled.
            title,
            overview: clean(d.description),
            production_year: date.as_deref().and_then(leading_year),
            // Only a FULL date sets a premiere date; a bare `1965` leaves it
            // unset rather than inventing the 1st of January.
            premiere_date: date.as_deref().and_then(parse_date_unix),
            studios: publisher.map(|p| vec![p]).unwrap_or_default(),
            people: clean(d.author)
                .map(|name| {
                    vec![PersonRef {
                        name,
                        // A book's byline is authorship, and `Writer` is the
                        // Jellyfin PersonType a client renders as such.
                        kind: PersonKind::Writer,
                        role: Some("Author".into()),
                        ..Default::default()
                    }]
                })
                .unwrap_or_default(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn req<'a>(
        path: &'a std::path::Path,
        probe: &'a pharos_core::MediaProbe,
    ) -> MetadataRequest<'a> {
        MetadataRequest {
            path,
            kind: MediaKind::Book,
            probe,
            series: None,
        }
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#;

    fn write_epub(path: &std::path::Path, opf: &str) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(CONTAINER.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.finish().unwrap();
    }

    #[tokio::test]
    async fn an_epubs_descriptive_fields_become_ordinary_item_metadata() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Dune.epub");
        write_epub(
            &p,
            r#"<package><metadata>
              <dc:title>Dune</dc:title>
              <dc:creator>Frank Herbert</dc:creator>
              <dc:publisher>Chilton Books</dc:publisher>
              <dc:date>1965-08-01</dc:date>
              <dc:description>A desert planet.</dc:description>
            </metadata><manifest/></package>"#,
        );

        let probe = pharos_core::MediaProbe::default();
        let out = BookMetadataProvider::new()
            .fetch(&req(&p, &probe))
            .await
            .unwrap();

        assert_eq!(out.title.as_deref(), Some("Dune"));
        assert_eq!(out.overview.as_deref(), Some("A desert planet."));
        assert_eq!(
            out.production_year,
            Some(1965),
            "the year is what the library sorts and filters on"
        );
        assert!(
            out.premiere_date.is_some(),
            "a full dc:date sets a premiere date"
        );
        assert_eq!(
            out.studios,
            vec!["Chilton Books".to_string()],
            "Jellyfin has no publisher field; Studios is where a client shows it"
        );
        assert_eq!(out.people.len(), 1);
        assert_eq!(out.people[0].name, "Frank Herbert");
        assert_eq!(out.people[0].kind, PersonKind::Writer);
    }

    /// A bare year must not become the 1st of January. The distinction is
    /// visible: a client displays a premiere date, and a fabricated one is a
    /// lie the UI presents as fact.
    #[tokio::test]
    async fn a_year_only_date_sets_a_year_and_no_premiere_date() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Old.epub");
        write_epub(
            &p,
            r#"<package><metadata><dc:title>X</dc:title><dc:date>1965</dc:date></metadata>
               <manifest/></package>"#,
        );

        let probe = pharos_core::MediaProbe::default();
        let out = BookMetadataProvider::new()
            .fetch(&req(&p, &probe))
            .await
            .unwrap();
        assert_eq!(out.production_year, Some(1965));
        assert_eq!(out.premiere_date, None);
    }

    /// FR-007 — a title-less file yields no title HERE, so the filename
    /// provider (priority 10) supplies one and no book lists untitled.
    #[tokio::test]
    async fn a_title_less_book_yields_no_title_so_the_filename_can_win() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Some Book.epub");
        write_epub(
            &p,
            r#"<package><metadata><dc:creator>Anon</dc:creator></metadata><manifest/></package>"#,
        );

        let probe = pharos_core::MediaProbe::default();
        let out = BookMetadataProvider::new()
            .fetch(&req(&p, &probe))
            .await
            .unwrap();
        assert_eq!(
            out.title, None,
            "an empty title must be None, not Some(\"\") — a blank string would \
             WIN the merge and the filename fallback would never run"
        );
        assert_eq!(out.people.len(), 1, "the author was still read");
    }

    /// An unreadable file must not abort the merge: the book imports, titled
    /// from its filename.
    #[tokio::test]
    async fn an_unreadable_book_yields_an_empty_result_not_an_error() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Lying.epub");
        std::fs::write(&p, b"this is plain text").unwrap();

        let probe = pharos_core::MediaProbe::default();
        let out = BookMetadataProvider::new()
            .fetch(&req(&p, &probe))
            .await
            .expect("a parse failure is not an error for the resolver (V6)");
        assert_eq!(out, MetadataResult::default());
    }

    #[tokio::test]
    async fn a_comics_comicinfo_becomes_ordinary_item_metadata() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Batman 001.cbz");
        let f = std::fs::File::create(&p).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("ComicInfo.xml", opts).unwrap();
        zw.write_all(
            br#"<ComicInfo><Title>The Dark Knight Returns</Title><Writer>Frank Miller</Writer>
                <Publisher>DC Comics</Publisher><Summary>Bruce returns.</Summary>
                <Year>1986</Year><Month>2</Month><Day>1</Day></ComicInfo>"#,
        )
        .unwrap();
        zw.start_file("page01.jpg", opts).unwrap();
        zw.write_all(b"\xFF\xD8\xFF\xD9").unwrap();
        zw.finish().unwrap();

        let probe = pharos_core::MediaProbe::default();
        let out = BookMetadataProvider::new()
            .fetch(&req(&p, &probe))
            .await
            .unwrap();
        assert_eq!(out.title.as_deref(), Some("The Dark Knight Returns"));
        assert_eq!(out.overview.as_deref(), Some("Bruce returns."));
        assert_eq!(out.production_year, Some(1986));
        assert!(out.premiere_date.is_some());
        assert_eq!(out.studios, vec!["DC Comics".to_string()]);
        assert_eq!(out.people[0].name, "Frank Miller");
    }

    #[tokio::test]
    async fn only_books_are_supported() {
        let p = BookMetadataProvider::new();
        assert!(p.supports(MediaKind::Book));
        for kind in [MediaKind::Movie, MediaKind::Episode, MediaKind::Audio] {
            assert!(
                !p.supports(kind),
                "{kind:?} would be read as an archive on every file in the library"
            );
        }
    }

    #[test]
    fn a_year_is_taken_only_from_a_leading_four_digit_run() {
        assert_eq!(leading_year("1965"), Some(1965));
        assert_eq!(leading_year("1965-08-01"), Some(1965));
        assert_eq!(leading_year("1965-08-01T00:00:00Z"), Some(1965));
        assert_eq!(leading_year(" 1965 "), Some(1965));
        // Not a year: a two-digit or five-digit run would otherwise produce
        // "year 8" or "year 19658" and sort the library into nonsense.
        assert_eq!(leading_year("08/1965"), None);
        assert_eq!(leading_year("19658"), None);
        assert_eq!(leading_year("MCMLXV"), None);
    }
}
