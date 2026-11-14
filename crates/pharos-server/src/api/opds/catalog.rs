//! Which books the OPDS catalogue lists, and in what order (045). Pure over
//! a list of items, so the selection rules are tested without a store.
//!
//! A book is listed only when a reader can open what its link sends:
//! FR-002 omits `BookFormat::Unreadable` (Kindle DRM among them), a Kindle
//! file whose EPUB conversion is off, and anything not on local disk.

use pharos_core::{MediaItem, MediaKind, Origin};

/// What an acquisition link sends. The `format` label of
/// `pharos_opds_downloads_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Epub,
    Pdf,
    Comic,
    /// A DRM-free Kindle file, sent as the EPUB it converts to.
    KindleEpub,
}

impl Format {
    pub const ALL: [Self; 4] = [Self::Epub, Self::Pdf, Self::Comic, Self::KindleEpub];

    pub fn label(self) -> &'static str {
        match self {
            Self::Epub => "epub",
            Self::Pdf => "pdf",
            Self::Comic => "comic",
            Self::KindleEpub => "kindle_epub",
        }
    }

    /// The link's title: what a reader shows on its download button.
    pub fn title(self) -> &'static str {
        match self {
            Self::Epub | Self::KindleEpub => "EPUB",
            Self::Pdf => "PDF",
            Self::Comic => "Comic",
        }
    }
}

/// What `item`'s acquisition link would send, and its MIME type; `None` when
/// the catalogue must not list it. `converts` says whether Kindle conversion
/// is on (`AppState::books`).
pub fn openable(item: &MediaItem, converts: bool) -> Option<(Format, &'static str)> {
    if item.kind != MediaKind::Book {
        return None;
    }
    let book = item.book.as_ref()?;
    if !book.format.readable_by_client() {
        return None;
    }
    if !matches!(item.origin(), Origin::Local(_)) {
        return None;
    }
    if pharos_scanner::book::kindle::is_converted_kindle(&item.path, book.format) {
        return converts.then_some((Format::KindleEpub, "application/epub+zip"));
    }
    let mime = crate::api::jellyfin::download::content_type_for(&item.path);
    let format = match mime {
        "application/epub+zip" => Format::Epub,
        "application/pdf" => Format::Pdf,
        m if m.starts_with("application/vnd.comicbook") || m.starts_with("application/x-cb") => {
            Format::Comic
        }
        // A book whose file type the download would send as octet-stream:
        // a reader could not tell what it is.
        _ => return None,
    };
    Some((format, mime))
}

/// The author a book is filed under, trimmed; `None` for none.
pub fn author_of(item: &MediaItem) -> Option<&str> {
    item.book
        .as_ref()
        .and_then(|b| b.author.as_deref())
        .map(str::trim)
        .filter(|a| !a.is_empty())
}

/// The stable path key of an author: a hash of the name ignoring case, so
/// no name text reaches a URL.
pub fn author_key(name: &str) -> String {
    format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(name.trim().to_lowercase().as_bytes())
    )
}

/// By title (`sort_name`, the library's own title order), then id.
pub fn sort_by_title(books: &mut [MediaItem]) {
    books.sort_by_cached_key(|b| (pharos_core::sort_name(&b.title), b.id));
}

/// Newest first, then id.
pub fn sort_recent(books: &mut [MediaItem]) {
    books.sort_by_key(|b| (std::cmp::Reverse(b.created_at.unwrap_or(0)), b.id));
}

/// Each author with their key and how many of `books` they wrote, by name.
pub fn authors(books: &[MediaItem]) -> Vec<(String, String, usize)> {
    let mut by_key: std::collections::HashMap<String, (String, usize)> =
        std::collections::HashMap::new();
    for name in books.iter().filter_map(author_of) {
        let entry = by_key
            .entry(author_key(name))
            .or_insert_with(|| (name.to_owned(), 0));
        entry.1 = entry.1.saturating_add(1);
    }
    let mut out: Vec<(String, String, usize)> = by_key
        .into_iter()
        .map(|(key, (name, n))| (key, name, n))
        .collect();
    out.sort_by_cached_key(|(key, name, _)| (pharos_core::sort_name(name), key.clone()));
    out
}

/// Longest search term read, in characters; the rest is ignored.
pub const MAX_QUERY_CHARS: usize = 200;
/// Most search words used.
pub const MAX_QUERY_WORDS: usize = 10;

/// The words of a search term, as the library search splits them.
pub fn query_words(q: &str) -> Vec<String> {
    let q: String = q.chars().take(MAX_QUERY_CHARS).collect();
    let mut words = pharos_core::search_tokens(&q);
    words.truncate(MAX_QUERY_WORDS);
    words
}

/// Whether every word is found in the title, the author or the series,
/// ignoring case (the V558 rule the library search follows).
pub fn matches(item: &MediaItem, words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    let mut hay = item.title.to_lowercase();
    if let Some(book) = &item.book {
        for field in [&book.author, &book.series_name].into_iter().flatten() {
            hay.push('\n');
            hay.push_str(&field.to_lowercase());
        }
    }
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// Entries per page of a feed.
pub const PAGE_SIZE: usize = 50;

/// The index range of page `page` (1-based) of `len` entries, and the last
/// page's number (at least 1).
pub fn page_range(len: usize, page: usize) -> (std::ops::Range<usize>, usize) {
    let last = len.div_ceil(PAGE_SIZE).max(1);
    let start = page.saturating_sub(1).saturating_mul(PAGE_SIZE).min(len);
    let end = start.saturating_add(PAGE_SIZE).min(len);
    (start..end, last)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pharos_core::{BookFormat, BookMeta};

    fn book(id: u64, path: &str, format: BookFormat) -> MediaItem {
        MediaItem {
            id,
            path: path.into(),
            title: format!("Title {id}"),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_books_a_reader_can_open_are_listed() {
        let epub = book(1, "/m/a.epub", BookFormat::Epub);
        assert_eq!(
            openable(&epub, true),
            Some((Format::Epub, "application/epub+zip"))
        );
        let pdf = book(2, "/m/a.pdf", BookFormat::Pdf);
        assert_eq!(openable(&pdf, true), Some((Format::Pdf, "application/pdf")));
        let cbz = book(3, "/m/a.cbz", BookFormat::Comic);
        assert_eq!(
            openable(&cbz, true),
            Some((Format::Comic, "application/vnd.comicbook+zip"))
        );
        let drm = book(4, "/m/a.azw3", BookFormat::Unreadable);
        assert_eq!(openable(&drm, true), None, "Kindle DRM is never listed");
        let kindle = book(5, "/m/a.azw3", BookFormat::Epub);
        assert_eq!(
            openable(&kindle, true),
            Some((Format::KindleEpub, "application/epub+zip"))
        );
        assert_eq!(
            openable(&kindle, false),
            None,
            "no conversion, no EPUB to send"
        );
        let mut film = book(6, "/m/a.epub", BookFormat::Epub);
        film.kind = MediaKind::Movie;
        assert_eq!(openable(&film, true), None);
        let mut song = book(7, "/m/a.epub", BookFormat::Epub);
        song.kind = MediaKind::Audio;
        assert_eq!(openable(&song, true), None);
    }

    #[test]
    fn format_labels_are_distinct() {
        let labels: std::collections::HashSet<_> = Format::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(labels.len(), Format::ALL.len());
    }

    #[test]
    fn authors_group_ignoring_case_and_sort_by_name() {
        let mut a = book(1, "/m/a.epub", BookFormat::Epub);
        let mut b = book(2, "/m/b.epub", BookFormat::Epub);
        let mut c = book(3, "/m/c.epub", BookFormat::Epub);
        let d = book(4, "/m/d.epub", BookFormat::Epub);
        a.book.as_mut().unwrap().author = Some("Wren Example".into());
        b.book.as_mut().unwrap().author = Some(" wren example ".into());
        c.book.as_mut().unwrap().author = Some("Ada Sample".into());
        let got = authors(&[a, b, c, d]);
        let names: Vec<_> = got
            .iter()
            .map(|(_, n, count)| (n.as_str(), *count))
            .collect();
        assert_eq!(names, [("Ada Sample", 1), ("Wren Example", 2)]);
        assert_eq!(got[1].0, author_key("WREN EXAMPLE"));
        assert!(got[1].0.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn search_needs_every_word_in_title_author_or_series() {
        let mut b = book(1, "/m/a.epub", BookFormat::Epub);
        b.title = "The Lantern Keeper".into();
        b.book.as_mut().unwrap().author = Some("Ada Sample".into());
        assert!(matches(&b, &query_words("lantern sample")));
        assert!(matches(&b, &query_words("KEEP")));
        assert!(!matches(&b, &query_words("lantern dragon")));
        assert!(
            !matches(&b, &query_words("")),
            "an empty search finds nothing"
        );
        assert_eq!(query_words(&"word ".repeat(50)).len(), MAX_QUERY_WORDS);
    }

    #[test]
    fn pages_cover_every_entry_once() {
        assert_eq!(page_range(0, 1), (0..0, 1));
        assert_eq!(page_range(120, 1), (0..50, 3));
        assert_eq!(page_range(120, 3), (100..120, 3));
        assert_eq!(page_range(120, 9), (120..120, 3), "past the end is empty");
        assert_eq!(page_range(120, 0), (0..50, 3));
    }

    #[test]
    fn title_order_ignores_articles_and_case() {
        let mut v = vec![
            book(1, "/m/1.epub", BookFormat::Epub),
            book(2, "/m/2.epub", BookFormat::Epub),
            book(3, "/m/3.epub", BookFormat::Epub),
        ];
        v[0].title = "the zebra".into();
        v[1].title = "Apple".into();
        v[2].title = "a Mango".into();
        sort_by_title(&mut v);
        let titles: Vec<_> = v.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, ["Apple", "a Mango", "the zebra"]);
    }
}
