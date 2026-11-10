//! Books kept on this device (spec 036 FR-004 to FR-006): what is recorded
//! about each, the cache names, the size rule and the checks that keep one
//! user's books from another. No I/O; `kept_web.rs` talks to Cache Storage
//! and `localStorage`.
//!
//! The record is one small `localStorage` value, so the offline start and
//! the title page read it without opening Cache Storage, and a device with
//! nothing kept has no value at all.

use pharos_client::api::{BookFormat, ItemRef, ReadPosition};
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

/// Why a book is on the device. A book kept by hand stays `Hand` however
/// often it is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Hand,
    /// "Keep the book I'm reading": while open and for a week after.
    Reading,
    /// "Keep my last N books".
    Recent,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hand => "kept by hand",
            Self::Reading => "reading now",
            Self::Recent => "recent",
        }
    }

    /// The word the record and the log use.
    pub fn word(self) -> &'static str {
        match self {
            Self::Hand => "hand",
            Self::Reading => "reading",
            Self::Recent => "recent",
        }
    }

    /// Whether the device may remove the book to make room.
    pub fn is_automatic(self) -> bool {
        self != Self::Hand
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
    /// When the book was last opened, unix milliseconds on this device's
    /// clock; what expiry and "oldest first" go by. 0 when never opened
    /// since it was kept.
    #[serde(default)]
    pub opened_ms: i64,
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

    /// Notes that the book was opened at `now_ms`, and why it is kept now
    /// when an automatic policy applies: a book kept by hand stays so.
    /// Returns whether the book is kept.
    pub fn note_open(&mut self, id: &str, now_ms: i64, reason: Option<Reason>) -> bool {
        let Some(book) = self.books.iter_mut().find(|b| b.id == id) else {
            return false;
        };
        book.opened_ms = now_ms;
        if let (true, Some(reason)) = (book.reason.is_automatic(), reason) {
            book.reason = reason;
        }
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

/// The place a kept book would open at with no word from the server, and
/// when it was read: the one noted in the record or the one waiting in the
/// save queue, whichever was read later (the queue's on a tie, as it is what
/// gets sent). `None` when neither has one.
fn local_place(book: &KeptBook, queued: Option<&PendingSave>) -> Option<(String, f32, i64)> {
    let noted = book
        .locator
        .as_ref()
        .map(|l| (l, book.fraction, book.read_at_ms));
    match (noted, queued) {
        (Some((l, f, at)), Some(q)) if at > q.read_at_ms => Some((l.clone(), f, at)),
        (_, Some(q)) => Some((q.locator.clone(), q.fraction, q.read_at_ms)),
        (Some((l, f, at)), None) => Some((l.clone(), f, at)),
        (None, None) => None,
    }
}

/// Where to resume a kept book with no word from the server.
pub fn resume_place(book: &KeptBook, queued: Option<&PendingSave>) -> Option<(String, f32)> {
    local_place(book, queued).map(|(locator, fraction, _)| (locator, fraction))
}

/// The server's place for a kept book that is already open, when the reader
/// should move to it: another device read further since this one noted its
/// own (spec 036 US4, the newest reading wins). `turned` is whether the user
/// has turned a page since the book opened; their own reading is newer than
/// anything the server held then, so they are never moved. Times are judged
/// to the whole second, as the server stores them.
pub fn server_place_to_adopt<'a>(
    book: &KeptBook,
    queued: Option<&PendingSave>,
    server: Option<&'a ReadPosition>,
    turned: bool,
) -> Option<&'a ReadPosition> {
    let server = server?;
    if turned || server.locator.is_empty() || server.read_at_ms <= 0 {
        return None;
    }
    match local_place(book, queued) {
        None => Some(server),
        Some((locator, _, at)) => (locator != server.locator
            && server.read_at_ms.div_euclid(1000) > at.div_euclid(1000))
        .then_some(server),
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

/// Where the automatic-keeping settings are, in `localStorage`. Per
/// device, never synced: what is kept is per device too (spec 036).
pub const SETTINGS_KEY: &str = "pharos.kept.settings";

/// How long a book stays "reading now" after it was last opened.
pub const READING_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The most books "Keep my last N books" can hold.
pub const MAX_LAST_N: u8 = 10;

/// The choices for the space automatic books may take, in MiB.
pub const BUDGET_CHOICES_MIB: [u32; 5] = [128, 256, 512, 1024, 2048];

const DEFAULT_BUDGET_MIB: u32 = 512;
const DEFAULT_LAST_N: u8 = 3;

/// The settings of automatic keeping (spec 036, Decisions). Read on every
/// book open and every opening of the list, so a change applies without a
/// reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoKeep {
    /// Keep the book I'm reading.
    pub reading: bool,
    /// Keep my last N books.
    pub recent: bool,
    pub last_n: u8,
    /// The space automatic books may take together, in MiB.
    pub budget_mib: u32,
}

impl Default for AutoKeep {
    fn default() -> Self {
        Self {
            reading: true,
            recent: false,
            last_n: DEFAULT_LAST_N,
            budget_mib: DEFAULT_BUDGET_MIB,
        }
    }
}

impl AutoKeep {
    /// The stored text, or the defaults when there is none or it is damaged.
    /// Values out of range are brought into it.
    pub fn parse(text: Option<&str>) -> Self {
        let read: Self = text
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();
        Self {
            last_n: read.last_n.clamp(1, MAX_LAST_N),
            budget_mib: if read.budget_mib == 0 {
                DEFAULT_BUDGET_MIB
            } else {
                read.budget_mib
            },
            ..read
        }
    }

    pub fn to_text(self) -> Option<String> {
        serde_json::to_string(&self).ok()
    }

    pub fn budget_bytes(&self) -> u64 {
        u64::from(self.budget_mib).saturating_mul(1024 * 1024)
    }

    /// Why a book opened now is kept, if a policy keeps it. Reading wins
    /// when both are on: every book opened in the last week is kept as
    /// reading, and the last-N rule covers only books kept while it was the
    /// one on.
    pub fn reason_on_open(&self) -> Option<Reason> {
        if self.reading {
            Some(Reason::Reading)
        } else if self.recent {
            Some(Reason::Recent)
        } else {
            None
        }
    }
}

/// Why an automatic book is not worth writing, or `None` when it is. The
/// weak-device cap applies to every kept book; a book bigger than the whole
/// budget would be removed the moment it was kept.
pub fn auto_keep_skip(bytes: u64, cap: Option<u64>, settings: &AutoKeep) -> Option<&'static str> {
    if cap.is_some_and(|cap| bytes > cap) {
        Some("over the device cap")
    } else if bytes > settings.budget_bytes() {
        Some("over the space budget")
    } else {
        None
    }
}

/// Why the device removes an automatic book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The policy that kept it is off now (FR-009).
    PolicyOff,
    /// Reading now, and not opened for a week.
    Expired,
    /// Recent, and beyond the last N.
    BeyondLast,
    /// Over the space for automatic books, and among the oldest opened.
    OverBudget,
}

impl Why {
    pub fn label(self) -> &'static str {
        match self {
            Self::PolicyOff => "policy-off",
            Self::Expired => "expired",
            Self::BeyondLast => "beyond-last",
            Self::OverBudget => "over-budget",
        }
    }
}

/// One book to remove, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eviction {
    pub id: String,
    pub why: Why,
    pub bytes: u64,
}

/// The automatic books to remove now. Books kept by hand are never in it and
/// do not count toward the budget. Reading-now books expire a week after
/// they were last opened; recent books beyond the last N go, then the
/// automatic books left are removed oldest-opened first while their total
/// is over the budget. A policy that is off removes the books it kept.
pub fn plan_eviction(books: &[KeptBook], settings: &AutoKeep, now_ms: i64) -> Vec<Eviction> {
    let mut gone = Vec::new();
    let mut left: Vec<&KeptBook> = Vec::new();
    for book in books {
        let why = match book.reason {
            Reason::Hand => continue,
            Reason::Reading if !settings.reading => Some(Why::PolicyOff),
            Reason::Reading if now_ms.saturating_sub(book.opened_ms) > READING_KEEP_MS => {
                Some(Why::Expired)
            }
            Reason::Recent if !settings.recent => Some(Why::PolicyOff),
            Reason::Reading | Reason::Recent => None,
        };
        match why {
            Some(why) => gone.push(Eviction {
                id: book.id.clone(),
                why,
                bytes: book.bytes,
            }),
            None => left.push(book),
        }
    }
    left.sort_by_key(|b| std::cmp::Reverse(b.opened_ms));
    let mut within_last = 0_usize;
    left.retain(|book| {
        if book.reason != Reason::Recent {
            return true;
        }
        within_last = within_last.saturating_add(1);
        if within_last <= usize::from(settings.last_n) {
            return true;
        }
        gone.push(Eviction {
            id: book.id.clone(),
            why: Why::BeyondLast,
            bytes: book.bytes,
        });
        false
    });
    let mut total = left
        .iter()
        .fold(0_u64, |sum, b| sum.saturating_add(b.bytes));
    let budget = settings.budget_bytes();
    // `left` is newest-opened first, so the oldest are at the back.
    while total > budget {
        let Some(book) = left.pop() else { break };
        total = total.saturating_sub(book.bytes);
        gone.push(Eviction {
            id: book.id.clone(),
            why: Why::OverBudget,
            bytes: book.bytes,
        });
    }
    gone
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
            opened_ms: 0,
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

    fn server(locator: &str, read_at_ms: i64) -> ReadPosition {
        ReadPosition {
            locator: locator.into(),
            fraction: 0.7,
            read_at_ms,
        }
    }

    fn noted(locator: &str, read_at_ms: i64) -> KeptBook {
        KeptBook {
            locator: Some(locator.into()),
            fraction: 0.2,
            read_at_ms,
            ..book("1", 10)
        }
    }

    #[test]
    fn a_server_place_read_later_is_adopted() {
        let kept = noted("mine", 100_000);
        let theirs = server("theirs", 160_000);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&theirs), false),
            Some(&theirs)
        );
        // The queue's place is the local one when it was read later than the record.
        assert_eq!(
            server_place_to_adopt(&kept, Some(&queued("q", 150_000)), Some(&theirs), false),
            Some(&theirs)
        );
        assert_eq!(
            server_place_to_adopt(&kept, Some(&queued("q", 170_000)), Some(&theirs), false),
            None
        );
        // A kept book with no place yet takes the server's.
        assert_eq!(
            server_place_to_adopt(&book("1", 10), None, Some(&theirs), false),
            Some(&theirs)
        );
    }

    #[test]
    fn a_local_place_read_later_stays() {
        let kept = noted("mine", 200_000);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("theirs", 100_000)), false),
            None
        );
    }

    #[test]
    fn the_same_second_is_equal_and_the_next_is_not() {
        let kept = noted("mine", 100_400);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("theirs", 100_000)), false),
            None
        );
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("theirs", 100_999)), false),
            None
        );
        assert!(
            server_place_to_adopt(&kept, None, Some(&server("theirs", 101_000)), false).is_some()
        );
    }

    #[test]
    fn a_server_with_no_place_or_no_time_changes_nothing() {
        let kept = noted("mine", 100_000);
        assert_eq!(server_place_to_adopt(&kept, None, None, false), None);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("", 0)), false),
            None
        );
        // A server older than schemaVersion 39 sends no time to judge by.
        assert_eq!(
            server_place_to_adopt(&book("1", 10), None, Some(&server("theirs", 0)), false),
            None
        );
    }

    #[test]
    fn the_same_place_needs_no_move() {
        let kept = noted("same", 100_000);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("same", 900_000)), false),
            None
        );
    }

    #[test]
    fn a_reader_who_has_turned_pages_is_not_moved() {
        let kept = noted("mine", 100_000);
        assert_eq!(
            server_place_to_adopt(&kept, None, Some(&server("theirs", 900_000)), true),
            None
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
        assert_eq!(Reason::Reading.label(), "reading now");
        assert_eq!(Reason::Recent.label(), "recent");
        assert_eq!(Reason::Reading.word(), "reading");
    }

    #[test]
    fn the_reasons_are_stored_as_words() {
        let mut store = KeptStore::new("u");
        store.keep(KeptBook {
            reason: Reason::Reading,
            ..book("1", 10)
        });
        store.keep(KeptBook {
            reason: Reason::Recent,
            ..book("2", 10)
        });
        let text = store.to_text().unwrap();
        assert!(text.contains(r#""reason":"reading""#), "{text}");
        assert!(text.contains(r#""reason":"recent""#), "{text}");
        assert_eq!(KeptStore::parse(&text), Ok(Some(store)));
    }

    #[test]
    fn a_record_without_an_open_time_reads_as_never_opened() {
        let text = r#"{"user":"u","books":[{"id":"1","title":"T","format":"epub","bytes":5,
            "kept_at":"2026-11-10T09:00:00Z","reason":"hand"}]}"#;
        let store = KeptStore::parse(text).unwrap().unwrap();
        assert_eq!(store.books[0].opened_ms, 0);
    }

    #[test]
    fn opening_notes_the_time_and_only_an_automatic_book_changes_its_reason() {
        let mut store = KeptStore::new("u");
        store.keep(book("hand", 10));
        store.keep(auto("rec", Reason::Recent, 10, 1));
        assert!(store.note_open("hand", 500, Some(Reason::Reading)));
        assert!(store.note_open("rec", 600, Some(Reason::Reading)));
        assert_eq!(store.find("hand").unwrap().reason, Reason::Hand);
        assert_eq!(store.find("hand").unwrap().opened_ms, 500);
        assert_eq!(store.find("rec").unwrap().reason, Reason::Reading);
        assert!(store.note_open("rec", 700, None));
        assert_eq!(store.find("rec").unwrap().reason, Reason::Reading);
        assert_eq!(store.find("rec").unwrap().opened_ms, 700);
        assert!(!store.note_open("missing", 1, None));
    }

    #[test]
    fn the_defaults_keep_the_book_being_read_and_nothing_else() {
        let s = AutoKeep::default();
        assert!(s.reading && !s.recent);
        assert_eq!((s.last_n, s.budget_mib), (3, 512));
        assert_eq!(s.budget_bytes(), 512 * MIB);
        assert_eq!(s.reason_on_open(), Some(Reason::Reading));
        assert!(BUDGET_CHOICES_MIB.contains(&s.budget_mib));
    }

    #[test]
    fn the_reason_a_book_is_kept_on_open_follows_the_policies() {
        let both = AutoKeep {
            recent: true,
            ..AutoKeep::default()
        };
        assert_eq!(both.reason_on_open(), Some(Reason::Reading));
        let only_recent = AutoKeep {
            reading: false,
            recent: true,
            ..AutoKeep::default()
        };
        assert_eq!(only_recent.reason_on_open(), Some(Reason::Recent));
        let neither = AutoKeep {
            reading: false,
            ..AutoKeep::default()
        };
        assert_eq!(neither.reason_on_open(), None);
    }

    #[test]
    fn the_settings_round_trip_and_a_missing_or_damaged_value_is_the_defaults() {
        let s = AutoKeep {
            reading: false,
            recent: true,
            last_n: 7,
            budget_mib: 2048,
        };
        let text = s.to_text().unwrap();
        assert_eq!(AutoKeep::parse(Some(&text)), s);
        assert_eq!(AutoKeep::parse(None), AutoKeep::default());
        assert_eq!(AutoKeep::parse(Some("{")), AutoKeep::default());
        assert_eq!(
            AutoKeep::parse(Some(r#"{"recent":true}"#)),
            AutoKeep {
                recent: true,
                ..AutoKeep::default()
            }
        );
    }

    #[test]
    fn a_count_or_budget_out_of_range_is_brought_in() {
        let low = AutoKeep::parse(Some(r#"{"last_n":0,"budget_mib":0}"#));
        assert_eq!((low.last_n, low.budget_mib), (1, 512));
        let high = AutoKeep::parse(Some(r#"{"last_n":99}"#));
        assert_eq!(high.last_n, MAX_LAST_N);
    }

    #[test]
    fn a_book_over_the_cap_or_the_whole_budget_is_not_worth_writing() {
        let s = AutoKeep {
            budget_mib: 128,
            ..AutoKeep::default()
        };
        assert_eq!(auto_keep_skip(10 * MIB, Some(64 * MIB), &s), None);
        assert_eq!(auto_keep_skip(10 * MIB, None, &s), None);
        assert_eq!(
            auto_keep_skip(65 * MIB, Some(64 * MIB), &s),
            Some("over the device cap")
        );
        assert_eq!(
            auto_keep_skip(129 * MIB, None, &s),
            Some("over the space budget")
        );
        assert_eq!(auto_keep_skip(128 * MIB, None, &s), None);
    }

    const MIB: u64 = 1024 * 1024;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 100 * DAY_MS;

    fn auto(id: &str, reason: Reason, bytes: u64, opened_ms: i64) -> KeptBook {
        KeptBook {
            reason,
            opened_ms,
            ..book(id, bytes)
        }
    }

    fn plan(books: &[KeptBook], settings: &AutoKeep) -> Vec<(String, Why)> {
        plan_eviction(books, settings, NOW)
            .into_iter()
            .map(|e| (e.id, e.why))
            .collect()
    }

    fn on(reading: bool, recent: bool) -> AutoKeep {
        AutoKeep {
            reading,
            recent,
            last_n: 2,
            budget_mib: 512,
        }
    }

    #[test]
    fn nothing_kept_or_only_fresh_books_within_budget_removes_nothing() {
        assert!(plan(&[], &on(true, true)).is_empty());
        let books = [
            auto("1", Reason::Reading, MIB, NOW - DAY_MS),
            auto("2", Reason::Recent, MIB, NOW - 2 * DAY_MS),
        ];
        assert!(plan(&books, &on(true, true)).is_empty());
    }

    #[test]
    fn a_reading_book_expires_a_week_after_it_was_last_opened() {
        let s = on(true, false);
        let books = [
            auto("edge", Reason::Reading, MIB, NOW - READING_KEEP_MS),
            auto("old", Reason::Reading, MIB, NOW - READING_KEEP_MS - 1),
        ];
        assert_eq!(plan(&books, &s), [("old".to_owned(), Why::Expired)]);
    }

    #[test]
    fn a_recent_book_does_not_expire_by_age() {
        let books = [auto("1", Reason::Recent, MIB, NOW - 90 * DAY_MS)];
        assert!(plan(&books, &on(false, true)).is_empty());
    }

    #[test]
    fn recent_books_beyond_the_last_n_go_and_the_newest_opened_stay() {
        let books = [
            auto("a", Reason::Recent, MIB, NOW - 3000),
            auto("b", Reason::Recent, MIB, NOW - 1000),
            auto("c", Reason::Recent, MIB, NOW - 2000),
        ];
        assert_eq!(
            plan(&books, &on(false, true)),
            [("a".to_owned(), Why::BeyondLast)]
        );
    }

    #[test]
    fn reading_books_are_not_counted_against_the_last_n() {
        let books = [
            auto("r1", Reason::Reading, MIB, NOW - 10),
            auto("r2", Reason::Reading, MIB, NOW - 20),
            auto("c1", Reason::Recent, MIB, NOW - 30),
            auto("c2", Reason::Recent, MIB, NOW - 40),
        ];
        assert!(plan(&books, &on(true, true)).is_empty());
    }

    #[test]
    fn turning_a_policy_off_removes_exactly_the_books_it_kept() {
        let books = [
            auto("r", Reason::Reading, MIB, NOW - 10),
            auto("c", Reason::Recent, MIB, NOW - 20),
            book("h", MIB),
        ];
        assert_eq!(
            plan(&books, &on(false, true)),
            [("r".to_owned(), Why::PolicyOff)]
        );
        assert_eq!(
            plan(&books, &on(true, false)),
            [("c".to_owned(), Why::PolicyOff)]
        );
        assert_eq!(
            plan(&books, &on(false, false)),
            [
                ("r".to_owned(), Why::PolicyOff),
                ("c".to_owned(), Why::PolicyOff)
            ]
        );
    }

    #[test]
    fn over_the_budget_the_oldest_opened_automatic_book_goes_first() {
        let s = AutoKeep {
            budget_mib: 128,
            ..on(true, true)
        };
        let books = [
            auto("mid", Reason::Reading, 60 * MIB, NOW - 2000),
            auto("new", Reason::Recent, 60 * MIB, NOW - 1000),
            auto("old", Reason::Reading, 60 * MIB, NOW - 3000),
        ];
        assert_eq!(plan(&books, &s), [("old".to_owned(), Why::OverBudget)]);
        let tight = AutoKeep {
            budget_mib: 100,
            ..s
        };
        assert_eq!(
            plan(&books, &tight),
            [
                ("old".to_owned(), Why::OverBudget),
                ("mid".to_owned(), Why::OverBudget)
            ]
        );
    }

    #[test]
    fn exactly_the_budget_is_within_it() {
        let s = AutoKeep {
            budget_mib: 128,
            ..on(true, false)
        };
        let books = [
            auto("1", Reason::Reading, 64 * MIB, NOW - 1),
            auto("2", Reason::Reading, 64 * MIB, NOW - 2),
        ];
        assert!(plan(&books, &s).is_empty());
    }

    #[test]
    fn books_kept_by_hand_are_never_removed_and_take_no_budget() {
        let s = AutoKeep {
            budget_mib: 128,
            ..on(true, true)
        };
        let books = [
            book("big-hand", 2000 * MIB),
            auto("auto", Reason::Reading, 100 * MIB, NOW - 1),
        ];
        assert!(plan(&books, &s).is_empty());
        assert!(plan(&books, &on(false, false))
            .iter()
            .all(|(id, _)| id != "big-hand"));
    }

    #[test]
    fn a_book_removed_for_one_reason_does_not_also_count_against_the_budget() {
        let s = AutoKeep {
            budget_mib: 128,
            ..on(true, false)
        };
        let books = [
            auto("expired", Reason::Reading, 100 * MIB, NOW - 8 * DAY_MS),
            auto("fresh", Reason::Reading, 100 * MIB, NOW - 1),
        ];
        assert_eq!(plan(&books, &s), [("expired".to_owned(), Why::Expired)]);
    }

    #[test]
    fn an_eviction_says_the_bytes_it_frees() {
        let books = [auto("1", Reason::Reading, 5 * MIB, NOW - 8 * DAY_MS)];
        let out = plan_eviction(&books, &on(true, false), NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, 5 * MIB);
        assert_eq!(Why::Expired.label(), "expired");
    }
}
