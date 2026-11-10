//! Books kept on this device (spec 036 FR-004 to FR-006): what is recorded
//! about each, the cache names, the size rule and the checks that keep one
//! user's books from another. No I/O; `kept_web.rs` talks to Cache Storage
//! and `localStorage`.
//!
//! The record is one small `localStorage` value, so the offline start and
//! the title page read it without opening Cache Storage, and a device with
//! nothing kept has no value at all.

use pharos_client::api::{BookFormat, ItemRef};
use serde::{Deserialize, Serialize};

use super::reader::{download_href, PendingSave};

/// Where the record of kept books is, in `localStorage`.
pub const STORE_KEY: &str = "pharos.kept";

/// Every cache of kept books starts with this. The service worker never
/// opens one (ADR-0110, V1133).
pub const CACHE_PREFIX: &str = "pharos-books-";

/// The cache a user's books are in. The user id is the server's (the hex
/// form `Session.me` reports), so two users never share a cache.
pub fn cache_name(user: &str) -> String {
    format!("{CACHE_PREFIX}{user}")
}

/// Whether `name` is a cache of kept books, of any user.
pub fn is_books_cache(name: &str) -> bool {
    name.starts_with(CACHE_PREFIX)
}

/// The request a book's bytes are stored under: the URL the reader would
/// fetch, which names the item.
pub fn book_key(id: ItemRef) -> String {
    download_href(id)
}

/// The formats that can be kept: the ones the reader opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeptFormat {
    Epub,
    Pdf,
}

impl KeptFormat {
    pub fn of(format: BookFormat) -> Option<Self> {
        match format {
            BookFormat::Epub => Some(Self::Epub),
            BookFormat::Pdf => Some(Self::Pdf),
            BookFormat::Comic | BookFormat::Unreadable => None,
        }
    }

    pub fn book_format(self) -> BookFormat {
        match self {
            Self::Epub => BookFormat::Epub,
            Self::Pdf => BookFormat::Pdf,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Epub => "EPUB",
            Self::Pdf => "PDF",
        }
    }
}

/// Why a book is on the device. Slice 4 adds the automatic reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Hand,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hand => "kept by hand",
        }
    }
}

/// One kept book: enough to list it and open it with no server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeptBook {
    /// As in a URL (`42`), see [`ItemRef`].
    pub id: String,
    pub title: String,
    pub format: KeptFormat,
    /// The size of the stored bytes.
    pub bytes: u64,
    /// ISO 8601, UTC.
    pub kept_at: String,
    pub reason: Reason,
    /// The last place read on this device, as the reader saves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    #[serde(default)]
    pub fraction: f32,
    /// When that place was read, unix milliseconds on this device's clock,
    /// as the save queue stamps its entries.
    #[serde(default)]
    pub read_at_ms: i64,
}

impl KeptBook {
    pub fn item(&self) -> Option<ItemRef> {
        self.id.parse().ok()
    }
}

/// Everything kept on this device, for one user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeptStore {
    /// The user who kept these; names the cache.
    pub user: String,
    pub books: Vec<KeptBook>,
}

impl KeptStore {
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_owned(),
            books: Vec::new(),
        }
    }

    /// Reads the stored text. An empty record is no record.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        let store: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok((!store.books.is_empty()).then_some(store))
    }

    /// The text to store, or `None` when nothing is kept (the value is then
    /// removed rather than written empty).
    pub fn to_text(&self) -> Option<String> {
        if self.books.is_empty() {
            return None;
        }
        serde_json::to_string(self).ok()
    }

    pub fn find(&self, id: &str) -> Option<&KeptBook> {
        self.books.iter().find(|b| b.id == id)
    }

    /// Adds `book`, replacing an earlier copy of the same item. Newest first.
    pub fn keep(&mut self, book: KeptBook) {
        self.books.retain(|b| b.id != book.id);
        self.books.insert(0, book);
    }

    /// Drops the book; whether it was there.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.books.len();
        self.books.retain(|b| b.id != id);
        self.books.len() != before
    }

    /// Notes the place last read, unless a later one is noted. Returns
    /// whether the book is kept.
    pub fn note_position(
        &mut self,
        id: &str,
        locator: &str,
        fraction: f32,
        read_at_ms: i64,
    ) -> bool {
        let Some(book) = self.books.iter_mut().find(|b| b.id == id) else {
            return false;
        };
        if book.locator.is_some() && read_at_ms < book.read_at_ms {
            return true;
        }
        book.locator = Some(locator.to_owned());
        book.fraction = fraction;
        book.read_at_ms = read_at_ms;
        true
    }

    pub fn total_bytes(&self) -> u64 {
        self.books
            .iter()
            .fold(0, |sum, b| sum.saturating_add(b.bytes))
    }

    /// Whether these books belong to `user`, who is signed in now.
    pub fn is_for(&self, user: &str) -> bool {
        self.user == user
    }
}

/// Where to resume a kept book with no word from the server: the place
/// noted in the record or the one waiting in the save queue, whichever was
/// read later (the queue's on a tie, as it is what gets sent). `None` when
/// neither has one.
pub fn resume_place(book: &KeptBook, queued: Option<&PendingSave>) -> Option<(String, f32)> {
    let noted = book
        .locator
        .as_ref()
        .map(|l| (l, book.fraction, book.read_at_ms));
    match (noted, queued) {
        (Some((l, f, at)), Some(q)) if at > q.read_at_ms => Some((l.clone(), f)),
        (_, Some(q)) => Some((q.locator.clone(), q.fraction)),
        (Some((l, f, _)), None) => Some((l.clone(), f)),
        (None, None) => None,
    }
}

/// The line shown when a book is over the cap, or `None` when it may be
/// kept. The same cap as opening (spec 036 FR-004).
pub fn keep_refusal(bytes: u64, cap: Option<u64>) -> Option<String> {
    let cap = cap?;
    (bytes > cap).then(|| {
        format!(
            "This book is {}, more than the {} this device keeps offline.",
            size_label(bytes),
            size_label(cap)
        )
    })
}

/// A size for people: whole MB above 10 MB, a decimal below, kB under 1 MB.
pub fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= 10.0 * MB {
        format!("{:.0} MB", b / MB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} kB", (b / KB).ceil())
    }
}

/// The line over the list: how many books and how much space.
pub fn total_label(store: &KeptStore) -> String {
    let n = store.books.len();
    format!(
        "{n} {} · {}",
        if n == 1 { "book" } else { "books" },
        size_label(store.total_bytes())
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn book(id: &str, bytes: u64) -> KeptBook {
        KeptBook {
            id: id.into(),
            title: format!("Invented Book {id}"),
            format: KeptFormat::Epub,
            bytes,
            kept_at: "2026-11-10T09:00:00Z".into(),
            reason: Reason::Hand,
            locator: None,
            fraction: 0.0,
            read_at_ms: 0,
        }
    }

    #[test]
    fn a_users_books_live_in_a_cache_named_for_them() {
        assert_eq!(cache_name("ab12"), "pharos-books-ab12");
        assert_ne!(cache_name("ab12"), cache_name("cd34"));
        assert!(is_books_cache(&cache_name("ab12")));
        assert!(!is_books_cache("pharos-shell-0123"));
        assert!(!is_books_cache("other-pharos-books-x"));
    }

    #[test]
    fn a_book_is_stored_under_its_download_url() {
        assert_eq!(book_key(ItemRef::Real(42)), "/Items/42/Download");
        assert_eq!(book_key(ItemRef::Synth(7)), "/Items/s7/Download");
    }

    #[test]
    fn only_the_formats_the_reader_opens_can_be_kept() {
        assert_eq!(KeptFormat::of(BookFormat::Epub), Some(KeptFormat::Epub));
        assert_eq!(KeptFormat::of(BookFormat::Pdf), Some(KeptFormat::Pdf));
        assert_eq!(KeptFormat::of(BookFormat::Comic), None);
        assert_eq!(KeptFormat::of(BookFormat::Unreadable), None);
    }

    #[test]
    fn the_record_round_trips_and_an_empty_one_is_no_record() {
        let mut store = KeptStore::new("ab12");
        assert_eq!(store.to_text(), None);
        store.keep(book("42", 1000));
        store.note_position("42", "epub:3/0.5", 0.4, 100);
        let text = store.to_text().expect("a book is kept");
        assert_eq!(KeptStore::parse(&text), Ok(Some(store)));
        assert_eq!(KeptStore::parse(r#"{"user":"ab12","books":[]}"#), Ok(None));
    }

    #[test]
    fn a_record_without_the_position_fields_still_reads() {
        let text = r#"{"user":"u","books":[{"id":"1","title":"T","format":"pdf","bytes":5,
            "kept_at":"2026-11-10T09:00:00Z","reason":"hand"}]}"#;
        let store = KeptStore::parse(text).expect("parses").expect("not empty");
        let b = &store.books[0];
        assert_eq!(
            (b.locator.as_deref(), b.fraction, b.read_at_ms),
            (None, 0.0, 0)
        );
        assert_eq!(b.format, KeptFormat::Pdf);
    }

    #[test]
    fn a_damaged_record_is_an_error_not_an_empty_list() {
        assert!(KeptStore::parse("{").is_err());
        assert!(KeptStore::parse(r#"{"user":"u","books":[{"id":"1"}]}"#).is_err());
    }

    #[test]
    fn keeping_again_replaces_and_removing_says_whether_it_was_there() {
        let mut store = KeptStore::new("u");
        store.keep(book("1", 10));
        store.keep(book("2", 20));
        store.keep(book("1", 30));
        let ids: Vec<_> = store.books.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
        assert_eq!(store.find("1").map(|b| b.bytes), Some(30));
        assert!(store.remove("2"));
        assert!(!store.remove("2"));
        assert_eq!(store.books.len(), 1);
    }

    #[test]
    fn a_position_is_noted_only_for_a_kept_book_and_never_goes_back_in_time() {
        let mut store = KeptStore::new("u");
        store.keep(book("1", 10));
        assert!(store.note_position("1", "epub:2/0.1", 0.2, 200));
        assert!(!store.note_position("9", "epub:2/0.1", 0.2, 200));
        assert!(store.note_position("1", "epub:1/0.0", 0.1, 100));
        let kept = store.find("1").expect("kept");
        assert_eq!(kept.locator.as_deref(), Some("epub:2/0.1"));
        assert_eq!(kept.read_at_ms, 200);
    }

    fn queued(locator: &str, read_at_ms: i64) -> PendingSave {
        PendingSave {
            locator: locator.into(),
            fraction: 0.5,
            read_at_ms,
        }
    }

    #[test]
    fn the_place_read_last_wins_between_the_record_and_the_queue() {
        let mut kept = book("1", 10);
        assert_eq!(resume_place(&kept, None), None);
        assert_eq!(
            resume_place(&kept, Some(&queued("q", 50))),
            Some(("q".into(), 0.5))
        );
        kept.locator = Some("noted".into());
        kept.fraction = 0.25;
        kept.read_at_ms = 100;
        assert_eq!(resume_place(&kept, None), Some(("noted".into(), 0.25)));
        assert_eq!(
            resume_place(&kept, Some(&queued("older", 99))),
            Some(("noted".into(), 0.25))
        );
        assert_eq!(
            resume_place(&kept, Some(&queued("newer", 101))),
            Some(("newer".into(), 0.5))
        );
        assert_eq!(
            resume_place(&kept, Some(&queued("tie", 100))),
            Some(("tie".into(), 0.5))
        );
    }

    #[test]
    fn the_total_adds_every_book_and_does_not_overflow() {
        let mut store = KeptStore::new("u");
        store.keep(book("1", 1_500_000));
        store.keep(book("2", 2_500_000));
        assert_eq!(store.total_bytes(), 4_000_000);
        store.keep(book("3", u64::MAX));
        assert_eq!(store.total_bytes(), u64::MAX);
    }

    #[test]
    fn books_belong_to_the_user_who_kept_them() {
        let store = KeptStore::new("ab12");
        assert!(store.is_for("ab12"));
        assert!(!store.is_for("cd34"));
        assert!(!store.is_for(""));
    }

    #[test]
    fn the_weak_device_cap_refuses_a_big_book_and_says_why() {
        use super::super::reader::WEAK_DEVICE_MAX_BYTES as CAP;
        assert_eq!(keep_refusal(CAP, Some(CAP)), None);
        assert_eq!(keep_refusal(CAP * 10, None), None);
        let line = keep_refusal(CAP + 1, Some(CAP)).expect("over the cap");
        assert!(line.contains("64 MB") && line.contains("offline"), "{line}");
    }

    #[test]
    fn sizes_read_for_people() {
        assert_eq!(size_label(0), "0 kB");
        assert_eq!(size_label(1), "1 kB");
        assert_eq!(size_label(500 * 1024), "500 kB");
        assert_eq!(size_label(1_572_864), "1.5 MB");
        assert_eq!(size_label(64 * 1024 * 1024), "64 MB");
        assert_eq!(size_label(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn the_total_line_counts_books_and_space() {
        let mut store = KeptStore::new("u");
        store.keep(book("1", 1_572_864));
        assert_eq!(total_label(&store), "1 book · 1.5 MB");
        store.keep(book("2", 1_572_864));
        assert_eq!(total_label(&store), "2 books · 3.0 MB");
    }

    #[test]
    fn the_reason_reads_as_the_spec_words_it() {
        assert_eq!(Reason::Hand.label(), "kept by hand");
    }
}
