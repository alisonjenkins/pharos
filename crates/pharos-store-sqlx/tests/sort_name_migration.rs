//! Migration 0054 (`media_items.sort_name`) on a database that already holds
//! rows: the column is added, existing rows are backfilled by the connect
//! step, a rerun changes nothing, and new writes store their own sort name.

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_store_sqlx::sqlite::SqliteStore;

mod common;

use common::{migrated_to, scratch_db_path};

/// The last migration before `sort_name` was added.
const BEFORE_SORT_NAME: i64 = 53;

/// A database at schema 0053 holding `titles` as movies with ids 0..n.
async fn populated_old_db(url: &str, titles: &[&str]) {
    let pool = migrated_to(url, BEFORE_SORT_NAME).await;
    // One transaction: a file-backed autocommit insert per row costs an fsync
    // each, which took over a minute for 1,200 rows on a loaded CI disk.
    let mut tx = pool.begin().await.unwrap();
    for (i, title) in titles.iter().enumerate() {
        sqlx::query("INSERT INTO media_items (id, path, title, kind) VALUES (?, ?, ?, 'movie')")
            .bind(i64::try_from(i).unwrap())
            .bind(format!("/m/{i}.mkv"))
            .bind(*title)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    pool.close().await;
}

async fn sort_names(store: &SqliteStore) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT id, sort_name FROM media_items ORDER BY id")
        .fetch_all(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_backfills_existing_rows_and_is_idempotent() {
    let path = scratch_db_path("sort-name-migration");
    let url = format!("sqlite://{}", path.display());
    // More rows than one backfill batch (500) so the keyset paging runs.
    let mut titles: Vec<String> = (0..1_203).map(|i| format!("Filler {i}")).collect();
    titles[0] = "The Matrix".into();
    titles[1] = "Ep 10".into();
    titles[2] = "Ep 2".into();
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    populated_old_db(&url, &refs).await;

    let store = SqliteStore::connect_without_backfill(&url).await.unwrap();
    let before = sort_names(&store).await;
    assert_eq!(before.len(), 1_203);
    assert!(
        before.iter().all(|(_, s)| s.is_none()),
        "migrating alone must not rewrite rows"
    );

    assert_eq!(store.backfill_sort_names().await.unwrap(), 1_203);
    let rows = sort_names(&store).await;
    assert!(
        rows.iter().all(|(_, s)| s.is_some()),
        "every pre-existing row, id 0 included, must be backfilled"
    );
    assert_eq!(rows[0].0, 0);
    assert_eq!(rows[0].1.as_deref(), Some("matrix"));
    assert_eq!(rows[1].1.as_deref(), Some("ep 00000000000000000010"));
    assert_eq!(rows[2].1.as_deref(), Some("ep 00000000000000000002"));

    // A rerun finds nothing to fill and leaves the values alone.
    assert_eq!(store.backfill_sort_names().await.unwrap(), 0);
    assert_eq!(sort_names(&store).await, rows);
    store.pool().close().await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn connect_fills_old_rows_in_the_background() {
    let path = scratch_db_path("sort-name-migration");
    let url = format!("sqlite://{}", path.display());
    populated_old_db(&url, &["The Matrix", "Ep 2"]).await;

    let store = SqliteStore::connect(&url).await.unwrap();
    // `connect` returns before the backfill necessarily finished.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let rows = sort_names(&store).await;
        if rows.iter().all(|(_, s)| s.is_some()) {
            assert_eq!(rows[0].1.as_deref(), Some("matrix"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background backfill never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    store.pool().close().await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unfilled_rows_sort_by_title_fold_until_backfilled() {
    use pharos_core::{MediaQuery, SortDir, SortKey};

    let store = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    for (id, title) in [(1u64, "Alpha"), (2, "The Matrix"), (3, "Nemo"), (4, "Zeta")] {
        store
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // Put "The Matrix" back into the pre-migration state.
    sqlx::query("UPDATE media_items SET sort_name = NULL WHERE id = 2")
        .execute(store.pool())
        .await
        .unwrap();
    let order = |store: &SqliteStore| {
        let q = MediaQuery {
            sort: vec![(SortKey::SortName, SortDir::Asc)],
            ..Default::default()
        };
        let store = store.clone();
        async move {
            store
                .query(&q)
                .await
                .unwrap()
                .0
                .into_iter()
                .map(|i| i.title)
                .collect::<Vec<_>>()
        }
    };
    // The unfilled row keeps its plain-fold position (under T) mid-backfill.
    assert_eq!(
        order(&store).await,
        vec!["Alpha", "Nemo", "The Matrix", "Zeta"]
    );
    store.backfill_sort_names().await.unwrap();
    assert_eq!(
        order(&store).await,
        vec!["Alpha", "The Matrix", "Nemo", "Zeta"]
    );
}

#[tokio::test]
async fn put_stores_the_sort_name_for_new_and_rewritten_items() {
    let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let item = |title: &str| MediaItem {
        id: 7,
        path: "/m/7.mkv".into(),
        title: title.into(),
        kind: MediaKind::Movie,
        ..Default::default()
    };
    store.put(item("The Matrix")).await.unwrap();
    assert_eq!(sort_names(&store).await[0].1.as_deref(), Some("matrix"));
    store.put(item("A Quiet Place")).await.unwrap();
    assert_eq!(
        sort_names(&store).await[0].1.as_deref(),
        Some("quiet place")
    );
}

#[tokio::test]
async fn fts_reindexes_on_title_change_but_not_on_sort_name_updates() {
    let store = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    let item = |title: &str| MediaItem {
        id: 3,
        path: "/m/3.mkv".into(),
        title: title.into(),
        kind: MediaKind::Movie,
        ..Default::default()
    };
    let hits = |term: &'static str| {
        let pool = store.pool().clone();
        async move {
            sqlx::query_as::<_, (i64,)>("SELECT rowid FROM media_fts WHERE media_fts MATCH ?")
                .bind(term)
                .fetch_all(&pool)
                .await
                .unwrap()
                .len()
        }
    };
    store.put(item("Alpha")).await.unwrap();
    assert_eq!(hits("alpha").await, 1);
    store.put(item("Beta")).await.unwrap();
    assert_eq!((hits("alpha").await, hits("beta").await), (0, 1));

    sqlx::query("UPDATE media_items SET sort_name = 'changed' WHERE id = 3")
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(hits("beta").await, 1);

    let (sql,): (String,) = sqlx::query_as(
        "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'media_fts_au'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        sql.contains("UPDATE OF title, overview"),
        "the FTS update trigger must be column-scoped, got: {sql}"
    );
}

/// Postgres runs the same assertions whenever `PHAROS_TEST_POSTGRES_URL` is
/// set (`just test-postgres`): the INSERT's `$N` arity is not compile-checked.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_stores_and_backfills_sort_names() {
    use pharos_store_sqlx::postgres::PostgresStore;

    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_stores_and_backfills_sort_names: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let store = PostgresStore::connect_without_backfill(&url).await.unwrap();
    let id = 910_054_i64;
    store
        .put(MediaItem {
            id: id as u64,
            path: "/m/sort-name-pg-put.mkv".into(),
            title: "The Matrix".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let put: (Option<String>,) = sqlx::query_as("SELECT sort_name FROM media_items WHERE id = $1")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(put.0.as_deref(), Some("matrix"));

    // A row that predates the column: NULL until the next connect.
    let old_id = id + 1;
    sqlx::query(
        "INSERT INTO media_items (id, path, title, kind) VALUES ($1, $2, 'An Ep 2', 'movie') \
         ON CONFLICT (id) DO UPDATE SET sort_name = NULL",
    )
    .bind(old_id)
    .bind("/m/sort-name-pg-old.mkv")
    .execute(store.pool())
    .await
    .unwrap();
    let again = PostgresStore::connect_without_backfill(&url).await.unwrap();
    again.backfill_sort_names().await.unwrap();
    let filled: (Option<String>,) =
        sqlx::query_as("SELECT sort_name FROM media_items WHERE id = $1")
            .bind(old_id)
            .fetch_one(again.pool())
            .await
            .unwrap();
    assert_eq!(filled.0.as_deref(), Some("ep 00000000000000000002"));
}
