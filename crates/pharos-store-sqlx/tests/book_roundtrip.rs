//! 004-books (T018) — `BookMeta` survives the store, on BOTH backends.
//!
//! Postgres is not optional theatre here. `MEDIA_COLUMNS`, the INSERT column
//! list, the `$N` placeholder run and the `MediaRow` field order are FOUR
//! separate hand-maintained lists per backend, and sqlx checks none of them at
//! compile time. A book written with a mis-numbered placeholder compiles, passes
//! `just test` (where the postgres arm skips itself) and fails only in CI. So
//! the same assertions run against a real postgres whenever
//! `PHAROS_TEST_POSTGRES_URL` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    BookFormat, BookMeta, MediaId, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaStore,
};

fn book(id: MediaId, title: &str, book: Option<BookMeta>) -> MediaItem {
    MediaItem {
        id,
        path: format!("/media/books/{id}.epub").into(),
        title: title.into(),
        kind: if book.is_some() {
            MediaKind::Book
        } else {
            MediaKind::Movie
        },
        book,
        // FR-001 — a book is never probed, so this stays default. Asserted
        // below, because a probe appearing on a book means something handed it
        // to ffmpeg.
        probe: MediaProbe::default(),
        series: None,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata::default(),
        has_primary_art: false,
        art_version: 0,
        match_provider: None,
        match_external_id: None,
        match_source: None,
        match_confidence: None,
        metadata_refreshed_at: None,
    }
}

fn fully_populated() -> BookMeta {
    BookMeta {
        format: BookFormat::Comic,
        page_count: Some(184),
        author: Some("Alan Moore".into()),
        publisher: Some("DC Comics".into()),
        series_name: Some("Watchmen".into()),
        series_index: Some(3),
        isbn: Some("978-0930289232".into()),
    }
}

/// Id base for this test's rows.
///
/// The postgres arm runs against a SHARED database that other test binaries
/// write to concurrently, so low ids collide: `backend_conformance` also writes
/// id 1, and whichever landed last decided whether this file's id-1 row was a
/// book or a movie. The failure looked like a store bug and was a test one.
const BASE: MediaId = 7_000;

async fn run<S: MediaStore>(store: S) {
    // 1. Every field survives, byte-identically.
    let full = book(BASE + 1, "Watchmen #3", Some(fully_populated()));
    store.put(full).await.unwrap();
    let got = store.get(BASE + 1).await.unwrap();
    assert_eq!(
        got.book.as_ref(),
        Some(&fully_populated()),
        "a fully-populated BookMeta did not round-trip"
    );
    assert_eq!(got.kind, MediaKind::Book);
    assert_eq!(
        got.probe,
        MediaProbe::default(),
        "FR-001 — a book carries no probe data"
    );

    // 2. A non-book item comes back with no BookMeta at all. If the seven
    //    columns were bound from anything other than the one Option, a movie
    //    would come back carrying a default BookMeta (format Epub!) instead of
    //    None.
    let movie = book(BASE + 2, "Alien", None);
    store.put(movie).await.unwrap();
    let got = store.get(BASE + 2).await.unwrap();
    assert_eq!(got.book, None, "a non-book item must carry no BookMeta");
    assert_eq!(got.kind, MediaKind::Movie);

    // 3. An unnumbered volume stays None and does not become 0. `Option<u32>`
    //    through an INTEGER column is exactly where a `unwrap_or(0)` would hide,
    //    and 0 would sort it FIRST in a series rather than last.
    let sparse = book(
        BASE + 3,
        "An unnumbered volume",
        Some(BookMeta {
            format: BookFormat::Epub,
            page_count: None,
            author: None,
            publisher: None,
            series_name: Some("Some Series".into()),
            series_index: None,
            isbn: None,
        }),
    );
    store.put(sparse).await.unwrap();
    let got = store.get(BASE + 3).await.unwrap().book.unwrap();
    assert_eq!(
        got.series_index, None,
        "an unnumbered volume must stay None"
    );
    assert_eq!(got.page_count, None, "epub has no stable page count");
    assert_eq!(got.series_name.as_deref(), Some("Some Series"));
    assert_eq!(got.format, BookFormat::Epub);

    // 4. Re-put updates in place (the upsert's book_* SET clauses). Without
    //    them a rescan that corrected an author would silently keep the old one.
    let mut corrected = fully_populated();
    corrected.author = Some("Alan Moore & Dave Gibbons".into());
    store
        .put(book(BASE + 1, "Watchmen #3", Some(corrected.clone())))
        .await
        .unwrap();
    assert_eq!(
        store.get(BASE + 1).await.unwrap().book,
        Some(corrected),
        "the upsert must update book_* columns, not just insert them"
    );

    // 5. Every BookFormat survives its own round-trip through the discriminator
    //    column, so no format decodes as another.
    for (i, format) in [
        BookFormat::Epub,
        BookFormat::Pdf,
        BookFormat::Comic,
        BookFormat::Unreadable,
    ]
    .into_iter()
    .enumerate()
    {
        let id = BASE + 100 + i as MediaId;
        store
            .put(book(
                id,
                format.as_str(),
                Some(BookMeta {
                    format,
                    ..Default::default()
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            store.get(id).await.unwrap().book.map(|b| b.format),
            Some(format),
            "{} did not survive the store",
            format.as_str()
        );
    }

    // 6. A book is reachable through the LIST path too, not only `get`. The list
    //    query selects MEDIA_COLUMNS separately, so a column missing from that
    //    string would show up here and nowhere else.
    let all = store.list().await.unwrap();
    let listed = all.iter().find(|i| i.id == BASE + 1).unwrap();
    assert!(
        listed.book.is_some(),
        "MEDIA_COLUMNS must carry book_* for the list path, not just for get"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_book_roundtrip() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    run(s).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_book_roundtrip() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_book_roundtrip: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let p = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    run(p).await;
}

/// Migration 0052 patches the `kind` CHECK constraint through
/// `PRAGMA writable_schema` because SQLite cannot ALTER one. That mechanism could
/// just as easily have DELETED the constraint as widened it, and the difference
/// is invisible from a passing round-trip: books would store either way.
///
/// So assert both halves. 'book' is accepted, and a bogus kind is still
/// rejected — proving the backstop survived the patch.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn widening_the_kind_check_did_not_delete_it() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");

    // The widened value is accepted through raw SQL (the typed path is covered
    // by the round-trip test above).
    sqlx::query("INSERT INTO media_items (id, path, title, kind) VALUES (?, ?, ?, 'book')")
        .bind(9001i64)
        .bind("/media/books/ok.epub")
        .bind("Accepted")
        .execute(s.pool())
        .await
        .expect("'book' must be accepted after migration 0052");

    // A value outside the set is still refused. If `replace()` had matched
    // nothing the constraint would be unchanged and the FIRST assertion would
    // fail; if the patch had dropped the constraint, THIS one fails. Only a
    // correct widening passes both.
    let bogus = sqlx::query(
        "INSERT INTO media_items (id, path, title, kind) VALUES (?, ?, ?, 'audiobook')",
    )
    .bind(9002i64)
    .bind("/media/books/bad.m4b")
    .bind("Rejected")
    .execute(s.pool())
    .await;
    let err = bogus.expect_err("an unknown kind must still violate the CHECK constraint");
    assert!(
        err.to_string().contains("CHECK constraint failed"),
        "expected a CHECK violation, got: {err}"
    );
}
