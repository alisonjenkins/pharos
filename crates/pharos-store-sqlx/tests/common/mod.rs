//! Fixtures for tests that build a file-backed SQLite database at an old
//! schema version and then let the store migrate it forward.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use std::borrow::Cow;
use std::str::FromStr;

pub fn scratch_db_path(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("pharos-{label}-{}-{nanos}.db", std::process::id()))
}

/// A pool on a new database at `url` with every migration up to and including
/// `last_version` applied.
///
/// The database is throwaway, so it skips fsync: replaying the migrations
/// fsyncs on every commit, and on a CI runner's disk (~0.25 s per fsync) that
/// took over 100 s. The store under test reopens the file with its
/// production settings afterwards.
pub async fn migrated_to(url: &str, last_version: i64) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Off);
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
            .filter(|m| m.version <= last_version)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    pool
}
