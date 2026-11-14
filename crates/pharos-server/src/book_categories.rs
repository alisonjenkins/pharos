//! Shelve the books whose own files named no usable subject, from Open
//! Library (spec 043, ADR-0129).
//!
//! A slow background pass, off unless `[books].online_subjects` is set. It
//! takes each book whose genres are empty or only `Other`, asks
//! [`OpenLibrary`] for its subjects, maps them to shelves with
//! `pharos_scanner::book::shelf`, and links the shelves as the book's genres.
//! `link_item_genres` keeps an admin's locked list, and a locked book is not
//! asked about at all.
//!
//! # Memo
//!
//! Every answer, and every miss, is kept in `.book_subjects.json` beside the
//! cache, keyed by the ISBN, title and author asked. A book shelved this way
//! loses its shelf when a rescan relinks genres from the file; the next pass
//! puts it back from the memo without a request. A miss is asked again after
//! [`MISS_TTL_SECS`]. A failed request is not kept, so the next pass retries.
//!
//! # Signals
//!
//! `sum by (source, outcome) (increase(pharos_book_category_total{source="openlibrary"}[1d]))`
//! and `{namespace="pharos"} |= "book categorised"`, which names the item, the
//! raw subjects, the shelves and the work matched, or the reason for a miss.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pharos_core::{DomainResult, EditableField, GenreStore, MediaId, MediaKind, MediaStore};
use pharos_scanner::book::shelf::{self, Outcome, Source};
use serde::{Deserialize, Serialize};

use crate::openlibrary::{BookQuery, Fetch, Lookup, OpenLibrary};

/// Delay before the first pass, so boot I/O and the first scan settle.
const WARMUP: Duration = Duration::from_secs(120);
/// Gap between passes. New books arrive with scans, a few a day.
const PASS_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Requests' worth of books asked per pass, so a first pass over a large
/// library banks its answers in bounded chunks.
const MAX_LOOKUPS_PER_PASS: usize = 300;
/// Failed requests in a row that end a pass: Open Library is down, and
/// asking about every remaining book would only add to its load.
const MAX_FAILURES_IN_A_ROW: usize = 3;
/// How long a miss is believed. Open Library's catalogue grows; a month is
/// long enough not to ask about the same book every pass.
pub const MISS_TTL_SECS: i64 = 30 * 24 * 3600;
/// Most raw subjects written to the log line per book.
const LOGGED_SUBJECTS: usize = 12;
/// Bumped when what an answer means changes, which drops every entry.
const MEMO_VERSION: u32 = 1;

/// One remembered answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoEntry {
    /// `None` is a miss.
    pub subjects: Option<Vec<String>>,
    pub work: Option<String>,
    /// Unix seconds.
    pub checked_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoDoc {
    version: u32,
    entries: HashMap<String, MemoEntry>,
}

/// Open Library's answers, kept across restarts. Losing the file costs one
/// round of requests, so any failure to read it is an empty memo.
#[derive(Debug)]
pub struct SubjectMemo {
    path: PathBuf,
    doc: RwLock<MemoDoc>,
}

impl SubjectMemo {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<MemoDoc>(&s).ok())
            .filter(|d| d.version == MEMO_VERSION)
            .unwrap_or(MemoDoc {
                version: MEMO_VERSION,
                entries: HashMap::new(),
            });
        Self {
            path,
            doc: RwLock::new(doc),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.doc.read().map_or(0, |d| d.entries.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, key: &str) -> Option<MemoEntry> {
        self.doc.read().ok()?.entries.get(key).cloned()
    }

    pub fn record(&self, key: String, entry: MemoEntry) {
        if let Ok(mut d) = self.doc.write() {
            d.entries.insert(key, entry);
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let json = {
            let Ok(d) = self.doc.read() else {
                return Ok(());
            };
            serde_json::to_string(&*d)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// The memo key for a query: what was asked, normalised.
fn memo_key(q: &BookQuery) -> String {
    format!(
        "{}|{}|{}",
        q.isbn_digits().unwrap_or_default(),
        crate::openlibrary::normalised_title(&q.title),
        shelf::normalise(q.author.as_deref().unwrap_or_default()),
    )
}

/// What one pass did, for the pass's log line and the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub asked: usize,
    pub from_memo: usize,
    pub shelved: usize,
    pub misses: usize,
    pub unavailable: usize,
    pub locked: usize,
}

/// State one job keeps between passes.
#[derive(Debug, Default)]
pub struct JobState {
    /// Locked books already counted, so a locked book is one `locked` per
    /// process, not one per pass.
    locked_seen: std::sync::Mutex<HashSet<MediaId>>,
}

/// Link `subjects`' shelves to `id`, then count and log the decision.
async fn apply<S: GenreStore>(
    store: &S,
    id: MediaId,
    current: &[String],
    subjects: &[String],
    via: &str,
    work: Option<&str>,
) -> DomainResult<Outcome> {
    let usable = shelf::usable_subjects(subjects);
    let shelves = shelf::shelves_for(&usable);
    let outcome = Outcome::of_shelves(&shelves);
    let names = shelf::names(&shelves);
    if !names.is_empty() && names != current {
        store.link_item_genres(id, &names).await?;
    }
    shelf::record(Source::OpenLibrary, outcome);
    tracing::info!(
        media.id = id,
        source = Source::OpenLibrary.label(),
        outcome = outcome.label(),
        via,
        work,
        subjects = ?usable.iter().take(LOGGED_SUBJECTS).collect::<Vec<_>>(),
        subject_count = usable.len(),
        shelves = ?names,
        "book categorised"
    );
    Ok(outcome)
}

/// One pass over the library's books.
pub async fn run_pass<S, F>(
    store: &S,
    client: &OpenLibrary<F>,
    memo: &SubjectMemo,
    state: &JobState,
    now: i64,
) -> DomainResult<PassReport>
where
    S: MediaStore + GenreStore,
    F: Fetch,
{
    let mut report = PassReport::default();
    let mut failures_in_a_row = 0usize;
    let books: Vec<_> = store
        .list()
        .await?
        .into_iter()
        .filter(|i| i.kind == MediaKind::Book)
        .collect();
    for item in books {
        let current = store.item_genre_names(item.id).await?;
        if !shelf::needs_lookup(&current) {
            continue;
        }
        if item.locked.contains(EditableField::Genres) {
            let first = state
                .locked_seen
                .lock()
                .map(|mut s| s.insert(item.id))
                .unwrap_or(false);
            if first {
                report.locked += 1;
                shelf::record(Source::OpenLibrary, Outcome::Locked);
                tracing::info!(
                    media.id = item.id,
                    source = Source::OpenLibrary.label(),
                    outcome = Outcome::Locked.label(),
                    "book categorised: genres locked by an admin, not looked up"
                );
            }
            continue;
        }
        let book = item.book.clone().unwrap_or_default();
        let query = BookQuery {
            isbn: book.isbn,
            title: item.title.trim().to_owned(),
            author: book.author.filter(|a| !a.trim().is_empty()),
        };
        if query.title.is_empty() {
            continue;
        }
        let key = memo_key(&query);
        match memo.get(&key) {
            Some(MemoEntry {
                subjects: Some(subjects),
                work,
                ..
            }) => {
                let would = shelf::names(&shelf::shelves_for(&subjects));
                if would.is_empty() || would == current {
                    // Already as the memo would leave it: not a new decision.
                    continue;
                }
                report.from_memo += 1;
                if apply(store, item.id, &current, &subjects, "memo", work.as_deref()).await?
                    == Outcome::Shelved
                {
                    report.shelved += 1;
                }
                continue;
            }
            Some(MemoEntry {
                subjects: None,
                checked_at,
                ..
            }) if now.saturating_sub(checked_at) < MISS_TTL_SECS => {
                tracing::debug!(
                    media.id = item.id,
                    "book lookup: a recent miss is remembered"
                );
                continue;
            }
            _ => {}
        }
        if report.asked >= MAX_LOOKUPS_PER_PASS {
            tracing::info!(
                cap = MAX_LOOKUPS_PER_PASS,
                "book lookup: pass cap reached; the rest wait for the next pass"
            );
            break;
        }
        report.asked += 1;
        match client.lookup(&query).await {
            Ok(Lookup::Found {
                subjects,
                via,
                work,
            }) => {
                failures_in_a_row = 0;
                memo.record(
                    key,
                    MemoEntry {
                        subjects: Some(subjects.clone()),
                        work: Some(work.clone()),
                        checked_at: now,
                    },
                );
                if apply(
                    store,
                    item.id,
                    &current,
                    &subjects,
                    via.label(),
                    Some(&work),
                )
                .await?
                    == Outcome::Shelved
                {
                    report.shelved += 1;
                }
            }
            Ok(Lookup::NoMatch { reason }) => {
                failures_in_a_row = 0;
                report.misses += 1;
                memo.record(
                    key,
                    MemoEntry {
                        subjects: None,
                        work: None,
                        checked_at: now,
                    },
                );
                shelf::record(Source::OpenLibrary, Outcome::NoMatch);
                tracing::info!(
                    media.id = item.id,
                    source = Source::OpenLibrary.label(),
                    outcome = Outcome::NoMatch.label(),
                    title = %query.title,
                    author = ?query.author,
                    isbn = ?query.isbn_digits(),
                    reason = %reason,
                    "book categorised: no confident match"
                );
            }
            Err(e) => {
                failures_in_a_row += 1;
                report.unavailable += 1;
                shelf::record(Source::OpenLibrary, Outcome::Unavailable);
                tracing::warn!(
                    media.id = item.id,
                    source = Source::OpenLibrary.label(),
                    outcome = Outcome::Unavailable.label(),
                    error = %e,
                    "book categorised: open library could not be asked"
                );
                if failures_in_a_row >= MAX_FAILURES_IN_A_ROW {
                    tracing::warn!(
                        failures = failures_in_a_row,
                        "book lookup: open library keeps failing; ending this pass"
                    );
                    break;
                }
            }
        }
    }
    Ok(report)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Start the job. Fire-and-forget: a failed pass is logged and the next one
/// runs on schedule.
pub fn spawn(stores: crate::state::Stores, bg_leader: Arc<AtomicBool>, memo_path: PathBuf) {
    let memo = SubjectMemo::load(memo_path);
    tracing::info!(
        memo = %memo.path().display(),
        remembered = memo.len(),
        user_agent = %crate::openlibrary::user_agent(),
        "book subjects: open library lookups on; delete the memo to ask again"
    );
    tokio::spawn(async move {
        let client = OpenLibrary::new();
        let state = JobState::default();
        tokio::time::sleep(WARMUP).await;
        // One replica asks, not each of them.
        while !bg_leader.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        loop {
            match run_pass(&stores, &client, &memo, &state, now_secs()).await {
                Ok(r) => tracing::info!(
                    asked = r.asked,
                    from_memo = r.from_memo,
                    shelved = r.shelved,
                    misses = r.misses,
                    unavailable = r.unavailable,
                    locked = r.locked,
                    "book subjects: pass done"
                ),
                Err(e) => tracing::warn!(error = %e, "book subjects: pass failed"),
            }
            if let Err(e) = memo.save() {
                tracing::warn!(path = %memo.path().display(), error = %e, "book subjects: memo not saved");
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::openlibrary::tests::{client, ok, status, FakeFetch};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use pharos_core::{BookFormat, BookMeta, MediaItem, MetadataEdit, MetadataEditStore};

    const NOW: i64 = 1_800_000_000;
    const WORK: &str = r#"{"subjects":["Computer security","Accessible book"]}"#;
    const EDITION: &str = r#"{"works":[{"key":"/works/OL5W"}]}"#;

    async fn store_with(books: &[(MediaId, &str, Option<&str>)]) -> crate::state::Stores {
        let s = crate::state::Stores::connect("sqlite::memory:")
            .await
            .unwrap();
        for (id, title, isbn) in books {
            s.put(MediaItem {
                id: *id,
                path: format!("/books/{id}.pdf").into(),
                title: (*title).into(),
                kind: MediaKind::Book,
                book: Some(BookMeta {
                    format: BookFormat::Pdf,
                    author: Some("Sam Sample".into()),
                    isbn: isbn.map(str::to_owned),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        s
    }

    fn memo() -> (tempfile::TempDir, SubjectMemo) {
        let dir = tempfile::tempdir().unwrap();
        let m = SubjectMemo::load(dir.path().join(".book_subjects.json"));
        (dir, m)
    }

    fn counted(snap: &metrics_util::debugging::Snapshotter, outcome: &str) -> u64 {
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_book_category_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "source" && l.value() == "openlibrary")
                    && key
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    }

    #[tokio::test]
    async fn a_found_book_is_shelved_and_remembered() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let store = store_with(&[(1, "Locked Doors", Some("9780000000019"))]).await;
        let c = client(
            FakeFetch::default()
                .on("/isbn/9780000000019.json", ok(EDITION))
                .on("/works/OL5W.json", ok(WORK)),
        );
        let (dir, memo) = memo();
        let state = JobState::default();
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!((r.asked, r.shelved), (1, 1), "{r:?}");
        assert_eq!(
            store.item_genre_names(1).await.unwrap(),
            vec!["Security".to_string()]
        );
        assert_eq!(counted(&snap, "shelved"), 1);

        // Shelved now: the next pass asks nothing.
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!(r, PassReport::default());

        // A rescan relinks from the file and drops the shelf; the memo puts it
        // back with no request.
        store.link_item_genres(1, &[]).await.unwrap();
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!((r.asked, r.from_memo, r.shelved), (0, 1, 1), "{r:?}");
        assert_eq!(
            store.item_genre_names(1).await.unwrap(),
            vec!["Security".to_string()]
        );
        assert_eq!(c_asked(&c), 2, "two requests in all: edition and work");

        // The memo survives a restart.
        memo.save().unwrap();
        let again = SubjectMemo::load(dir.path().join(".book_subjects.json"));
        assert_eq!(again.len(), 1);
    }

    fn c_asked(c: &OpenLibrary<FakeFetch>) -> usize {
        c.fetch_for_tests().asked().len()
    }

    #[tokio::test]
    async fn a_miss_is_counted_and_not_asked_again_for_a_month() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let store = store_with(&[(2, "Unknown Volume", None)]).await;
        let c = client(FakeFetch::default().on("/search.json", ok(r#"{"docs":[]}"#)));
        let (_dir, memo) = memo();
        let state = JobState::default();
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!((r.asked, r.misses), (1, 1));
        assert_eq!(counted(&snap, "no_match"), 1);
        assert!(store.item_genre_names(2).await.unwrap().is_empty());
        let r = run_pass(&store, &c, &memo, &state, NOW + 3600)
            .await
            .unwrap();
        assert_eq!(r.asked, 0, "a recent miss is not asked again");
        let r = run_pass(&store, &c, &memo, &state, NOW + MISS_TTL_SECS + 1)
            .await
            .unwrap();
        assert_eq!(r.asked, 1, "an old miss is asked again");
    }

    #[tokio::test]
    async fn a_failed_request_is_counted_and_retried_next_pass() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let store = store_with(&[(3, "Flaky Volume", None)]).await;
        let c = client(
            FakeFetch::default()
                .on("/search.json", status(500, ""))
                .on("/search.json", status(429, ""))
                .on("/search.json", status(429, "")),
        );
        let (_dir, memo) = memo();
        let state = JobState::default();
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!(r.unavailable, 1);
        assert!(memo.is_empty(), "a failure is not remembered");
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!(r.unavailable, 1, "throttled twice is a failure too");
        assert_eq!(counted(&snap, "unavailable"), 2);
        assert_eq!(c_asked(&c), 3, "500 once, then 429 and its one retry");
    }

    #[tokio::test]
    async fn repeated_failures_end_the_pass() {
        let books: Vec<(MediaId, String)> = (10..16).map(|i| (i, format!("Volume {i}"))).collect();
        let refs: Vec<(MediaId, &str, Option<&str>)> =
            books.iter().map(|(i, t)| (*i, t.as_str(), None)).collect();
        let store = store_with(&refs).await;
        let mut fetch = FakeFetch::default();
        for _ in 0..6 {
            fetch = fetch.on("/search.json", Err("connection refused".into()));
        }
        let c = client(fetch);
        let (_dir, memo) = memo();
        let r = run_pass(&store, &c, &memo, &JobState::default(), NOW)
            .await
            .unwrap();
        assert_eq!(r.unavailable, MAX_FAILURES_IN_A_ROW);
        assert_eq!(c_asked(&c), MAX_FAILURES_IN_A_ROW);
    }

    #[tokio::test]
    async fn a_locked_or_shelved_book_is_never_asked() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let store = store_with(&[(4, "Hand Shelved", None), (5, "File Shelved", None)]).await;
        store
            .edit_metadata(
                &pharos_core::EditTarget::Item(4),
                &MetadataEdit {
                    genres: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store.link_item_genres(5, &["Web".into()]).await.unwrap();
        let c = client(FakeFetch::default());
        let (_dir, memo) = memo();
        let state = JobState::default();
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!((r.asked, r.locked), (0, 1), "{r:?}");
        let r = run_pass(&store, &c, &memo, &state, NOW).await.unwrap();
        assert_eq!(r.locked, 0, "a locked book is counted once");
        assert_eq!(counted(&snap, "locked"), 1);
        assert_eq!(c_asked(&c), 0);
    }

    #[tokio::test]
    async fn a_book_on_other_is_looked_up() {
        let store = store_with(&[(6, "Odd Volume", Some("9780000000026"))]).await;
        store.link_item_genres(6, &["Other".into()]).await.unwrap();
        let c = client(
            FakeFetch::default()
                .on("/isbn/9780000000026.json", ok(EDITION))
                .on("/works/OL5W.json", ok(WORK)),
        );
        let (_dir, memo) = memo();
        let r = run_pass(&store, &c, &memo, &JobState::default(), NOW)
            .await
            .unwrap();
        assert_eq!(r.shelved, 1);
        assert_eq!(
            store.item_genre_names(6).await.unwrap(),
            vec!["Security".to_string()]
        );
    }

    #[test]
    fn the_memo_key_is_the_normalised_query() {
        let q = BookQuery {
            isbn: Some("978-0-00-000000-2".into()),
            title: "A Title: Its Subtitle".into(),
            author: Some("Sam  SAMPLE".into()),
        };
        assert_eq!(memo_key(&q), "9780000000002|a title|sam sample");
    }
}
