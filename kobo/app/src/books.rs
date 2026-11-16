//! Books as plain values: the listing, the download address, which files this
//! reader can open, and the record of what is downloaded. Nothing here touches
//! the runtime, so all of it is tested on the host.
//!
//! Pharos serves books through the same Jellyfin-compatible API as music. Its
//! OPDS catalogue wants HTTP Basic with an e-reader password, which this app
//! never holds; the Quick Connect token it does hold works on `/Items`.

use crate::api::{query_escape, Account, PAGE};
use crate::filters::Narrow;
use kobo_json::{ObjectBuilder, Value};

/// The store key of the downloaded-books record.
pub const KEY: &str = "books-v1";
/// The most a downloaded book may be: the most Cobalt's shelf takes in one
/// file. The whole file is held in memory to parse it, on a reader shared
/// with the stock software.
pub const MAX_BYTES: usize = kobo_sdk::MAX_SHELF_DOWNLOAD;
/// [`MAX_BYTES`] in the words the reader shows.
pub const TOO_BIG: &str = "This book is over the 64 MB limit.";
/// How much of a book one request fetches.
pub const CHUNK: u32 = 256 * 1024;
/// Downloaded books kept at once, so the record below stays small.
pub const MAX_DOWNLOADED: usize = 64;
/// The largest a reading position may be and still be kept.
const MAX_MEMORY: usize = 16 * 1024;
/// The largest the whole downloaded-books record may be.
const RECORD_LIMIT: usize = 192 * 1024;

/// What this reader can do with a book's file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Support {
    /// Opens, saved under this extension.
    Reads(Kind),
    /// Does not open; the words say why.
    Cannot(&'static str),
}

/// The formats `kobo-doc` reads that Pharos serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Epub,
    Text,
    Markdown,
    Html,
}

impl Kind {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Epub => "epub",
            Self::Text => "txt",
            Self::Markdown => "md",
            Self::Html => "html",
        }
    }
}

/// Which readable kind the item's `Path` names. Pharos hands a Kindle book
/// over as the EPUB it converted, and `Path` says so, so those read as EPUB.
/// Pharos never converts KFX, so a KFX book would arrive as itself.
#[must_use]
pub fn support(path: &str) -> Support {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "epub" | "azw" | "azw3" | "mobi" => Support::Reads(Kind::Epub),
        "kfx" => Support::Cannot("This reader can't open KFX Kindle books yet."),
        "txt" => Support::Reads(Kind::Text),
        "md" | "markdown" => Support::Reads(Kind::Markdown),
        "htm" | "html" | "xhtml" => Support::Reads(Kind::Html),
        "pdf" => Support::Cannot("This reader can't open PDF books yet."),
        "cbz" | "cbr" | "cbt" | "cb7" => Support::Cannot("This reader can't open comic books yet."),
        _ => Support::Cannot("This reader can't open this kind of book yet."),
    }
}

/// Whether `path` names a Kindle book Pharos converts.
#[must_use]
pub fn is_kindle(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        ["azw", "azw3", "mobi"]
            .iter()
            .any(|kindle| extension.eq_ignore_ascii_case(kindle))
    })
}

/// Whether downloaded bytes are what `kind` promised. An EPUB is a zip; a
/// Kindle book Pharos could not convert arrives as itself, and the parser
/// would take those bytes for prose and show garbage.
#[must_use]
pub fn is_what_it_claims(kind: Kind, bytes: &[u8]) -> bool {
    !bytes.is_empty() && (kind != Kind::Epub || bytes.starts_with(b"PK\x03\x04"))
}

/// One book in the listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Book {
    pub id: String,
    pub title: String,
    /// Empty when Pharos sent no author.
    pub author: String,
    pub support: Support,
    /// Whether the file is a Kindle book, which Pharos hands over as an EPUB
    /// only when it could convert it.
    pub kindle: bool,
    /// Pharos's tag for the cover, present only when there is a cover to
    /// fetch.
    pub image_tag: Option<String>,
    /// The description, as plain text; empty when Pharos sent none.
    pub overview: String,
}

impl crate::shelf::Item for Book {
    fn id(&self) -> &str {
        &self.id
    }

    fn image_tag(&self) -> Option<&str> {
        self.image_tag.as_deref()
    }

    fn sort_name(&self) -> &str {
        &self.title
    }
}

impl Book {
    /// The name of the book's file on the shelf, or `None` when its id could
    /// not be used as one or the book cannot be read.
    #[must_use]
    pub fn file(&self) -> Option<String> {
        let Support::Reads(kind) = self.support else {
            return None;
        };
        let name = format!("book-{}.{}", self.id.to_ascii_lowercase(), kind.extension());
        kobo_sdk::is_valid_key(&name).then_some(name)
    }
}

/// A page of the listing and how many books there are in all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BookPage {
    pub books: Vec<Book>,
    pub total: usize,
}

/// Books in every library the user can see, narrowed and ordered by
/// `narrow` (V1451). `People` carries the author, `Path` the file's extension
/// and `Overview` the description. Books no client can open, a Kindle book
/// locked with DRM among them, are always left out (V1449). Only downloaded
/// books are `downloaded`, the ids of the books on this reader.
#[must_use]
pub fn list_url(
    server: &str,
    account: &Account,
    start: usize,
    narrow: &Narrow,
    downloaded: &[&str],
) -> String {
    let ids = if narrow.downloaded {
        let ids: Vec<String> = downloaded.iter().map(|id| query_escape(id)).collect();
        format!("&Ids={}", ids.join(","))
    } else {
        String::new()
    };
    format!(
        "{server}/Users/{}/Items?IncludeItemTypes=Book&Recursive=true&{}\
         &Fields=Path,People,Overview{ids}&StartIndex={start}&Limit={PAGE}",
        query_escape(&account.user_id),
        narrow.query(),
    )
}

/// The books named by `ids`, with what [`list_url`] asks for of each.
#[must_use]
pub fn by_ids_url(server: &str, account: &Account, ids: &[&str]) -> String {
    let ids: Vec<String> = ids.iter().map(|id| query_escape(id)).collect();
    format!(
        "{server}/Users/{}/Items?Ids={}&Fields=Path,People,Overview",
        query_escape(&account.user_id),
        ids.join(",")
    )
}

/// The book's file. The runtime fetches it in pieces, with the account's
/// header on each.
#[must_use]
pub fn download_url(server: &str, book: &str) -> String {
    format!("{server}/Items/{}/Download", query_escape(book))
}

/// The book's first pages as plain text, with the account's header.
#[must_use]
pub fn opening_url(server: &str, book: &str) -> String {
    format!("{server}/Items/{}/Opening", query_escape(book))
}

fn author_of(item: &Value) -> String {
    let named = |value: &Value| {
        value
            .get("Name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    let authors: Vec<String> = item
        .get("People")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|person| person.get("Type").and_then(Value::as_str) == Some("Author"))
        .filter_map(named)
        .collect();
    if authors.is_empty() {
        item.get("AlbumArtist")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        authors.join(", ")
    }
}

/// A page of `Items`. Rows without an id are dropped; a missing name reads as
/// "Untitled" so a row is never blank.
#[must_use]
pub fn parse_page(bytes: &[u8]) -> Option<BookPage> {
    let reply = kobo_json::parse(std::str::from_utf8(bytes).ok()?).ok()?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let books: Vec<Book> = items
        .iter()
        .filter_map(|item| {
            Some(Book {
                id: item.get("Id").and_then(Value::as_str)?.to_owned(),
                title: item
                    .get("Name")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled")
                    .to_owned(),
                author: author_of(item),
                support: support(item.get("Path").and_then(Value::as_str).unwrap_or_default()),
                kindle: is_kindle(item.get("Path").and_then(Value::as_str).unwrap_or_default()),
                image_tag: item
                    .get("ImageTags")
                    .and_then(|tags| tags.get("Primary"))
                    .and_then(Value::as_str)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_owned),
                overview: item
                    .get("Overview")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            })
        })
        .collect();
    let total = reply
        .get("TotalRecordCount")
        .and_then(Value::as_i64)
        .and_then(|total| usize::try_from(total).ok())
        .unwrap_or(books.len());
    Some(BookPage { books, total })
}

/// What to do after one piece of a download arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Piece {
    /// A full piece came: ask for the next.
    More,
    /// A short piece came: the file is whole.
    Whole,
    /// The file is past what the reader can hold.
    TooBig,
}

/// A piece is full when the file may go on. A file that is an exact multiple
/// of the piece size ends with an empty piece, which the runtime reports as
/// the end of the file.
#[must_use]
pub fn after_piece(received_before: usize, piece: usize) -> Piece {
    let total = received_before.saturating_add(piece);
    if piece > CHUNK as usize || total > MAX_BYTES {
        Piece::TooBig
    } else if piece == CHUNK as usize {
        Piece::More
    } else {
        Piece::Whole
    }
}

/// A downloaded book: which file, how big it should be, and where the reader
/// stopped (`kobo_read::Memory`, encoded).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Saved {
    pub id: String,
    pub file: String,
    pub size: usize,
    pub memory: String,
}

/// Every downloaded book, as kept across restarts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Shelf {
    pub saved: Vec<Saved>,
}

impl Shelf {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Saved> {
        self.saved.iter().find(|saved| saved.id == id)
    }

    /// Adds a book, replacing an earlier download of the same one and keeping
    /// where the reader had stopped. Refuses past [`MAX_DOWNLOADED`].
    pub fn insert(&mut self, mut saved: Saved) -> bool {
        if let Some(old) = self.saved.iter_mut().find(|old| old.id == saved.id) {
            if old.file == saved.file {
                saved.memory.clone_from(&old.memory);
            }
            *old = saved;
            true
        } else if self.saved.len() < MAX_DOWNLOADED {
            self.saved.push(saved);
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, file: &str) {
        self.saved.retain(|saved| saved.file != file);
    }

    /// Keeps where the reader stopped, and moves the book to the end, where
    /// the most recently read one is (V1445). A position too big to keep is
    /// dropped rather than the record being refused.
    pub fn remember(&mut self, id: &str, memory: String) {
        let Some(at) = self.saved.iter().position(|saved| saved.id == id) else {
            return;
        };
        let mut saved = self.saved.remove(at);
        saved.memory = if memory.len() <= MAX_MEMORY {
            memory
        } else {
            String::new()
        };
        self.saved.push(saved);
    }

    /// The books opened on this reader, the most recently read first. A book
    /// downloaded and never opened has no place kept, and is not one.
    pub fn recent(&self) -> impl Iterator<Item = &Saved> {
        self.saved
            .iter()
            .rev()
            .filter(|saved| !saved.memory.is_empty())
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let books = self
            .saved
            .iter()
            .map(|saved| {
                ObjectBuilder::new()
                    .set("id", saved.id.as_str())
                    .set("file", saved.file.as_str())
                    .set("size", saved.size.to_string())
                    .set("memory", saved.memory.as_str())
                    .build()
            })
            .collect();
        ObjectBuilder::new()
            .set("version", "1")
            .set("books", Value::Array(books))
            .build()
            .to_json()
            .into_bytes()
    }

    /// `None` for a record this app cannot trust. The caller then leaves the
    /// stored one alone rather than writing an empty shelf over it.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > RECORD_LIMIT {
            return None;
        }
        let record = kobo_json::parse(std::str::from_utf8(bytes).ok()?).ok()?;
        if record.get("version").and_then(Value::as_str) != Some("1") {
            return None;
        }
        let books = record.get("books").and_then(Value::as_array)?;
        if books.len() > MAX_DOWNLOADED {
            return None;
        }
        let mut shelf = Self::default();
        for book in books {
            let text = |key: &str| book.get(key).and_then(Value::as_str);
            let saved = Saved {
                id: text("id")?.to_owned(),
                file: text("file")?.to_owned(),
                size: text("size")?.parse().ok()?,
                memory: text("memory")?.to_owned(),
            };
            if !kobo_sdk::is_valid_key(&saved.file) || saved.size == 0 || saved.size > MAX_BYTES {
                return None;
            }
            if !shelf.insert(saved) {
                return None;
            }
        }
        Some(shelf)
    }

    /// Whether the encoded record fits where it is kept.
    #[must_use]
    pub fn fits(&self) -> bool {
        self.encode().len() <= RECORD_LIMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            user_id: "u1".to_owned(),
            token: "t0k".to_owned(),
        }
    }

    fn book(id: &str, path: &str) -> Book {
        Book {
            id: id.to_owned(),
            title: "Book One".to_owned(),
            author: "Author A".to_owned(),
            support: support(path),
            kindle: is_kindle(path),
            image_tag: None,
            overview: String::new(),
        }
    }

    fn saved(id: &str, memory: &str) -> Saved {
        Saved {
            id: id.to_owned(),
            file: format!("book-{id}.epub"),
            size: 10,
            memory: memory.to_owned(),
        }
    }

    #[test]
    fn the_listing_asks_for_books_by_name_with_authors_and_paths() {
        let url = list_url("https://p.test", &account(), 50, &Narrow::default(), &[]);
        assert!(url.starts_with("https://p.test/Users/u1/Items?"));
        assert!(url.contains("IncludeItemTypes=Book"));
        assert!(
            url.contains("&IsReadable=true"),
            "locked books are left out"
        );
        assert!(url.contains("SortBy=SortName&SortOrder=Ascending"));
        assert!(url.contains("Fields=Path,People,Overview"));
        assert!(url.contains("StartIndex=50&Limit=50"));
    }

    #[test]
    fn a_narrowed_listing_carries_its_filters_and_downloaded_ids() {
        let narrow = Narrow {
            downloaded: true,
            read: crate::filters::ReadState::Unread,
            ..Narrow::default()
        };
        let url = list_url("https://p.test", &account(), 0, &narrow, &["b1", "b 2"]);
        assert!(url.contains("&IsReadable=true&IsPlayed=false&"), "{url}");
        assert!(url.contains("&Ids=b1,b%202&"), "{url}");
        let all = list_url("https://p.test", &account(), 0, &Narrow::default(), &["b1"]);
        assert!(!all.contains("Ids="), "only Downloaded names ids: {all}");
    }

    #[test]
    fn the_download_is_the_items_file_and_carries_no_token() {
        assert_eq!(
            download_url("https://p.test", "ab12"),
            "https://p.test/Items/ab12/Download"
        );
        assert_eq!(
            download_url("https://p.test", "a/b"),
            "https://p.test/Items/a%2Fb/Download"
        );
    }

    #[test]
    fn the_opening_is_the_items_first_pages_and_carries_no_token() {
        assert_eq!(
            opening_url("https://p.test", "ab12"),
            "https://p.test/Items/ab12/Opening"
        );
        assert_eq!(
            opening_url("https://p.test", "a/b"),
            "https://p.test/Items/a%2Fb/Opening"
        );
    }

    #[test]
    fn epubs_and_kindle_conversions_read_and_pdfs_and_comics_say_why_not() {
        assert_eq!(support("/b/Book One.EPUB"), Support::Reads(Kind::Epub));
        assert_eq!(support("/b/Book One.azw3"), Support::Reads(Kind::Epub));
        assert_eq!(
            support("/b/Book One.kfx"),
            Support::Cannot("This reader can't open KFX Kindle books yet.")
        );
        assert_eq!(support("/b/notes.txt"), Support::Reads(Kind::Text));
        assert_eq!(support("/b/notes.md"), Support::Reads(Kind::Markdown));
        assert_eq!(support("/b/page.xhtml"), Support::Reads(Kind::Html));
        assert_eq!(
            support("/b/Book One.pdf"),
            Support::Cannot("This reader can't open PDF books yet.")
        );
        assert_eq!(
            support("/b/Comic One.cbz"),
            Support::Cannot("This reader can't open comic books yet.")
        );
        assert!(matches!(support(""), Support::Cannot(_)));
        assert!(matches!(support("/b/no-extension"), Support::Cannot(_)));
    }

    #[test]
    fn only_a_zip_is_an_epub_and_nothing_empty_is_a_book() {
        assert!(is_what_it_claims(Kind::Epub, b"PK\x03\x04rest"));
        assert!(!is_what_it_claims(Kind::Epub, b"%PDF-1.7"));
        assert!(!is_what_it_claims(Kind::Epub, b""));
        assert!(is_what_it_claims(Kind::Text, b"Some prose."));
        assert!(!is_what_it_claims(Kind::Text, b""));
    }

    #[test]
    fn a_readable_book_gets_a_valid_shelf_name_and_others_get_none() {
        assert_eq!(
            book("AB12", "/b/x.epub").file().as_deref(),
            Some("book-ab12.epub")
        );
        assert_eq!(
            book("ab12", "/b/x.txt").file().as_deref(),
            Some("book-ab12.txt")
        );
        assert_eq!(book("ab12", "/b/x.pdf").file(), None);
        assert_eq!(book("../x", "/b/x.epub").file(), None);
        assert_eq!(book(&"a".repeat(80), "/b/x.epub").file(), None);
    }

    #[test]
    fn a_page_reads_authors_paths_and_totals_and_drops_rows_without_ids() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"Book One","Path":"/b/one.epub",
             "ImageTags":{"Primary":"c0"},
             "People":[{"Name":"Editor E","Type":"Editor"},
                       {"Name":"Author A","Type":"Author"},
                       {"Name":"Author B","Type":"Author"}]},
            {"Id":"b","Path":"/b/two.pdf","AlbumArtist":"Author C",
             "ImageTags":{"Primary":""}},
            {"Name":"no id"}
        ],"TotalRecordCount":120}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.total, 120);
        assert_eq!(page.books.len(), 2);
        assert_eq!(page.books[0].title, "Book One");
        assert_eq!(page.books[0].author, "Author A, Author B");
        assert_eq!(page.books[0].support, Support::Reads(Kind::Epub));
        assert_eq!(page.books[1].title, "Untitled");
        assert_eq!(page.books[1].author, "Author C");
        assert!(matches!(page.books[1].support, Support::Cannot(_)));
        assert_eq!(page.books[0].image_tag.as_deref(), Some("c0"));
        assert_eq!(page.books[1].image_tag, None, "an empty tag is no cover");
        assert_eq!(parse_page(b"not json"), None);
    }

    #[test]
    fn a_page_reads_each_book_s_description() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"Book One","Path":"/b/one.epub","Overview":"  A tale.\n"},
            {"Id":"b","Name":"Book Two","Path":"/b/two.epub"}
        ],"TotalRecordCount":2}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.books[0].overview, "A tale.");
        assert_eq!(page.books[1].overview, "", "no description is empty");
    }

    #[test]
    fn a_download_goes_on_while_pieces_are_full_and_stops_at_the_limit() {
        let chunk = CHUNK as usize;
        assert_eq!(after_piece(0, chunk), Piece::More);
        assert_eq!(after_piece(chunk, 10), Piece::Whole);
        assert_eq!(after_piece(chunk, 0), Piece::Whole);
        assert_eq!(after_piece(MAX_BYTES - chunk, chunk), Piece::More);
        assert_eq!(after_piece(MAX_BYTES, 1), Piece::TooBig);
        assert_eq!(MAX_BYTES, 64 * 1024 * 1024, "TOO_BIG names this size");
        assert_eq!(after_piece(0, chunk + 1), Piece::TooBig);
    }

    #[test]
    fn the_record_round_trips_and_junk_is_refused() {
        let mut shelf = Shelf::default();
        assert!(shelf.insert(saved("a", "at 2\n")));
        assert!(shelf.insert(saved("b", "")));
        assert_eq!(Shelf::decode(&shelf.encode()), Some(shelf.clone()));
        assert_eq!(Shelf::decode(b"{}"), None);
        assert_eq!(Shelf::decode(b"not json"), None);
        let future = String::from_utf8(shelf.encode())
            .unwrap_or_default()
            .replace("\"version\":\"1\"", "\"version\":\"99\"");
        assert_eq!(Shelf::decode(future.as_bytes()), None);
        let escaped = String::from_utf8(shelf.encode())
            .unwrap_or_default()
            .replace("book-a.epub", "../a.epub");
        assert_eq!(Shelf::decode(escaped.as_bytes()), None);
    }

    #[test]
    fn downloading_again_keeps_the_place_and_the_shelf_has_a_limit() {
        let mut shelf = Shelf::default();
        assert!(shelf.insert(saved("a", "at 2\n")));
        assert!(shelf.insert(Saved {
            size: 20,
            memory: String::new(),
            ..saved("a", "")
        }));
        assert_eq!(shelf.saved.len(), 1);
        assert_eq!(
            shelf.get("a").map(|saved| saved.memory.as_str()),
            Some("at 2\n")
        );
        assert_eq!(shelf.get("a").map(|saved| saved.size), Some(20));
        for index in 0..MAX_DOWNLOADED {
            let _ = shelf.insert(saved(&format!("x{index}"), ""));
        }
        assert_eq!(shelf.saved.len(), MAX_DOWNLOADED);
        assert!(!shelf.insert(saved("one-too-many", "")));
        assert!(shelf.fits());
    }

    #[test]
    fn removing_forgets_the_book_and_a_huge_place_is_dropped_not_kept() {
        let mut shelf = Shelf::default();
        assert!(shelf.insert(saved("a", "")));
        shelf.remember("a", "x".repeat(MAX_MEMORY + 1));
        assert_eq!(shelf.get("a").map(|saved| saved.memory.len()), Some(0));
        shelf.remember("a", "at 3\n".to_owned());
        assert_eq!(
            shelf.get("a").map(|saved| saved.memory.as_str()),
            Some("at 3\n")
        );
        shelf.remove("book-a.epub");
        assert_eq!(shelf.get("a"), None);
    }

    #[test]
    fn the_last_book_read_comes_first_and_an_unopened_one_not_at_all() {
        let mut shelf = Shelf::default();
        assert!(shelf.insert(saved("a", "at 2\n")));
        assert!(shelf.insert(saved("b", "at 9\n")));
        assert!(shelf.insert(saved("c", "")));
        let order = |shelf: &Shelf| {
            shelf
                .recent()
                .map(|saved| saved.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&shelf), ["b", "a"]);
        shelf.remember("a", "at 3\n".to_owned());
        assert_eq!(order(&shelf), ["a", "b"]);
        // The order is the record's, so it survives a restart.
        assert_eq!(
            Shelf::decode(&shelf.encode()).as_ref().map(order),
            Some(order(&shelf))
        );
    }

    #[test]
    fn books_are_asked_for_by_id_with_their_details() {
        assert_eq!(
            by_ids_url("https://p", &account(), &["b1", "b 2"]),
            "https://p/Users/u1/Items?Ids=b1,b%202&Fields=Path,People,Overview"
        );
    }
}
