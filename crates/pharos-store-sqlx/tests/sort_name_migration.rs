//! Migration 0054 (`media_items.sort_name`) on a database that already holds
//! rows: the column is added, existing rows are backfilled by the connect
//! step, a rerun changes nothing, and new writes store their own sort name.

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_store_sqlx::sqlite::SqliteStore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::borrow::Cow;
use std::str::FromStr;

/// The last migration before `sort_name` was added.
const BEFORE_SORT_NAME: i64 = 53;

fn scratch_db_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pharos-sort-name-migration-{}-{nanos}.db",
        std::process::id()
    ))
}

/// A database at schema 0053 holding `titles` as movies with ids 1..=n.
async fn populated_old_db(url: &str, titles: &[&str]) {
    let opts = SqliteConnectOptions::from_str(url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("./migrations/sqlite");
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= BEFORE_SORT_NAME)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    for (i, title) in titles.iter().enumerate() {
        sqlx::query("INSERT INTO media_items (id, path, title, kind) VALUES (?, ?, ?, 'movie')")
            .bind(i64::try_from(i).unwrap() + 1)
            .bind(format!("/m/{i}.mkv"))
            .bind(*title)
            .execute(&pool)
            .await
            .unwrap();
    }
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
    let path = scratch_db_path();
    let url = format!("sqlite://{}", path.display());
    // More rows than one backfill batch (500) so the keyset paging runs.
    let mut titles: Vec<String> = (0..1_203).map(|i| format!("Filler {i}")).collect();
    titles[0] = "The Matrix".into();
    titles[1] = "Ep 10".into();
    titles[2] = "Ep 2".into();
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    populated_old_db(&url, &refs).await;

    let store = SqliteStore::connect(&url).await.unwrap();
    let rows = sort_names(&store).await;
    assert_eq!(rows.len(), 1_203);
    assert!(
        rows.iter().all(|(_, s)| s.is_some()),
        "every pre-existing row must be backfilled"
    );
    assert_eq!(rows[0].1.as_deref(), Some("matrix"));
    assert_eq!(rows[1].1.as_deref(), Some("ep 00000000000000000010"));
    assert_eq!(rows[2].1.as_deref(), Some("ep 00000000000000000002"));
    store.pool().close().await;

    // A second connect finds nothing to fill and leaves the values alone.
    let again = SqliteStore::connect(&url).await.unwrap();
    assert_eq!(sort_names(&again).await, rows);
    again.pool().close().await;
    let _ = std::fs::remove_file(&path);
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
    let store = PostgresStore::connect(&url).await.unwrap();
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
    let again = PostgresStore::connect(&url).await.unwrap();
    let filled: (Option<String>,) =
        sqlx::query_as("SELECT sort_name FROM media_items WHERE id = $1")
            .bind(old_id)
            .fetch_one(again.pool())
            .await
            .unwrap();
    assert_eq!(filled.0.as_deref(), Some("ep 00000000000000000002"));
}
