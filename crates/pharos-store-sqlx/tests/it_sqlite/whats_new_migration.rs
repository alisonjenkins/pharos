//! Migration 0067 (ADR-0065): every account that exists when it runs gets
//! `whats_new.seen` = `0`, so only an account made later has no value and is
//! walked through first-run setup. A value already set is kept.

use pharos_core::{UserId, UserPrefStore};
use pharos_store_sqlx::sqlite::SqliteStore;

use crate::common;

const BEFORE_WHATS_NEW: i64 = 66;
const KEY: &str = "whats_new.seen";

async fn insert_user(pool: &sqlx::SqlitePool, id: UserId, name: &str) {
    sqlx::query("INSERT INTO users (id, name, password_hash, admin) VALUES (?, ?, 'x', 0)")
        .bind(id.0.as_bytes().to_vec())
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn existing_accounts_start_at_entry_zero_and_new_ones_have_none() {
    let path = common::scratch_db_path("whats-new-migration");
    let url = format!("sqlite://{}", path.display());
    let old = UserId(uuid::Uuid::from_u128(1));
    let kept = UserId(uuid::Uuid::from_u128(2));
    let pool = common::migrated_to(&url, BEFORE_WHATS_NEW).await;
    insert_user(&pool, old, "old").await;
    insert_user(&pool, kept, "kept").await;
    sqlx::query("INSERT INTO user_prefs (user_id, key, value, updated_at) VALUES (?, ?, ?, 0)")
        .bind(kept.0.as_bytes().to_vec())
        .bind(KEY)
        .bind(b"5".to_vec())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let store = SqliteStore::connect_without_backfill(&url).await.unwrap();
    assert_eq!(
        store.get_user_pref(old, KEY).await.unwrap(),
        Some(b"0".to_vec())
    );
    assert_eq!(
        store.get_user_pref(kept, KEY).await.unwrap(),
        Some(b"5".to_vec())
    );
    let new = UserId(uuid::Uuid::from_u128(3));
    insert_user(store.pool(), new, "new").await;
    assert_eq!(store.get_user_pref(new, KEY).await.unwrap(), None);
    store.pool().close().await;
    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_existing_accounts_start_at_entry_zero() {
    use pharos_store_sqlx::postgres::PostgresStore;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::Connection;
    let Ok(shared) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!(
            "SKIP postgres_existing_accounts_start_at_entry_zero: PHAROS_TEST_POSTGRES_URL unset"
        );
        return;
    };
    let (base, _) = shared.rsplit_once('/').expect("postgres url has a db path");
    let db = "pharos_whats_new_migrate";
    let mut admin = sqlx::postgres::PgConnection::connect(&format!("{base}/postgres"))
        .await
        .unwrap();
    // DROP/CREATE DATABASE cannot run inside a transaction.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {db}")))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&mut admin)
        .await
        .unwrap();
    let url = format!("{base}/{db}");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("./migrations/postgres");
    migrator.migrations = std::borrow::Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= BEFORE_WHATS_NEW)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    let old = UserId(uuid::Uuid::from_u128(1));
    sqlx::query("INSERT INTO users (id, name, password_hash, admin) VALUES ($1, 'old', 'x', 0)")
        .bind(old.0.as_bytes().to_vec())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let store = PostgresStore::connect_without_backfill(&url).await.unwrap();
    assert_eq!(
        store.get_user_pref(old, KEY).await.unwrap(),
        Some(b"0".to_vec())
    );
}
