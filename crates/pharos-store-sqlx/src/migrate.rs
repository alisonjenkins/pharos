//! One-shot SQLite -> Postgres data migration (`pharos admin db-migrate`).
//!
//! Copies every table in [`COPIED_TABLES`] from a live `SqliteStore` into a
//! freshly migrated (empty) `PostgresStore`. Ids are kept, so association
//! tables (item_genres, playlist_items, …) keep resolving. The copy runs as
//! a single postgres transaction, so any failure rolls back and leaves the
//! target untouched. After the copy, `setval` moves every id sequence past
//! the copied max id so later inserts don't collide. A final count check,
//! run after commit on both pools, confirms every row arrived.
//!
//! The copy follows the schema rather than a hand-kept column list (B1071):
//! every column the SQLite table has is copied, with each value decoded by
//! the type of its Postgres column. A column the target lacks, or a type the
//! copy cannot carry, is an error naming the table and column. A SQLite table
//! on neither [`COPIED_TABLES`] nor [`NOT_COPIED`] stops the migration before
//! anything is written, so a table added later cannot be dropped silently.
//!
//! Table order is FK-safe: parents first, then the tables that reference them.

use crate::postgres::PostgresStore;
use crate::sqlite::SqliteStore;
use crate::StoreError;
use sqlx::{PgPool, Postgres, Row, SqlitePool, Transaction};

/// Per-table row counts copied, in copy order.
pub struct MigrationReport {
    pub tables: Vec<(String, u64)>,
}

/// Every table the migration copies, parents before the tables that
/// reference them.
pub const COPIED_TABLES: &[&str] = &[
    // Parents.
    "users",
    "libraries",
    "genres",
    "people",
    "studios",
    "tags",
    "collections",
    "playlists",
    "media_items",
    "series_metadata",
    "series_overrides",
    "person_details",
    "metadata_translations",
    "system_identity",
    "server_secrets",
    "runtime_config",
    "named_config",
    "scan_runs",
    "sync_groups",
    // Dependents.
    "auth_tokens",
    "user_data",
    "user_configuration",
    "display_preferences",
    "user_prefs",
    "read_positions",
    "artwork",
    "item_genres",
    "item_people",
    "item_studios",
    "item_tags",
    "collection_items",
    "playlist_items",
    "episode_fingerprints",
    "media_segments",
    "media_segment_scans",
    "media_segments_snapshot",
    "subtitle_styling",
    "transcode_sessions",
];

/// SQLite tables the migration leaves behind on purpose, with the reason.
/// Each one is rebuilt or kept by the target itself.
pub const NOT_COPIED: &[(&str, &str)] = &[
    (
        "_sqlx_migrations",
        "each backend records its own migrations",
    ),
    ("sqlite_sequence", "SQLite's own AUTOINCREMENT bookkeeping"),
    (
        "language_codes",
        "the postgres migration seeds the same rows",
    ),
    (
        "media_item_languages",
        "derived: the postgres insert trigger indexes each copied item",
    ),
    (
        "media_fts",
        "SQLite full-text index; postgres derives search_tsv",
    ),
    ("media_fts_config", "SQLite full-text index shadow table"),
    ("media_fts_data", "SQLite full-text index shadow table"),
    ("media_fts_docsize", "SQLite full-text index shadow table"),
    ("media_fts_idx", "SQLite full-text index shadow table"),
];

/// Copy every table in [`COPIED_TABLES`] from `src` (SQLite) into `dst`
/// (Postgres). `dst` must be an empty database: the postgres schema
/// migrations already ran at `PostgresStore::connect` time, so the tables
/// exist but hold no rows. Row counts are checked on both sides before
/// returning. On any mismatch or sqlx error the insert transaction has
/// already rolled back, so a failed call leaves `dst` untouched.
pub async fn migrate_sqlite_to_postgres(
    src: &SqliteStore,
    dst: &PostgresStore,
) -> Result<MigrationReport, StoreError> {
    let sp = src.pool();
    let dp = dst.pool();
    refuse_unknown_tables(sp).await?;
    let mut tx = dp.begin().await.map_err(StoreError::Sqlx)?;

    let mut tables: Vec<(String, u64)> = Vec::with_capacity(COPIED_TABLES.len());
    for table in COPIED_TABLES {
        let n = copy_table(sp, &mut tx, table).await?;
        tables.push(((*table).to_string(), n));
    }

    tx.commit().await.map_err(StoreError::Sqlx)?;

    verify_counts(sp, dp, &tables).await?;

    Ok(MigrationReport { tables })
}

/// The SQLite tables on neither list. Empty when the lists cover the schema.
pub async fn unlisted_tables(sp: &SqlitePool) -> Result<Vec<String>, StoreError> {
    let names: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    Ok(names
        .into_iter()
        .map(|(n,)| n)
        .filter(|n| !COPIED_TABLES.contains(&n.as_str()))
        .filter(|n| !NOT_COPIED.iter().any(|(skip, _)| skip == n))
        .collect())
}

async fn refuse_unknown_tables(sp: &SqlitePool) -> Result<(), StoreError> {
    let unknown = unlisted_tables(sp).await?;
    if unknown.is_empty() {
        return Ok(());
    }
    Err(StoreError::Parse(format!(
        "migration does not know how to treat table(s) {}: add each to COPIED_TABLES or \
         NOT_COPIED in migrate.rs",
        unknown.join(", ")
    )))
}

/// How a value is read from SQLite and bound into Postgres, chosen by the
/// Postgres column's type. Integers bind as `i64` and floats as `f64`
/// whatever their width, since postgres assignment-casts a wider bind into
/// an INSERT's target column.
#[derive(Debug, Clone, Copy)]
enum Carry {
    Int,
    Float,
    Text,
    Bytes,
    /// SQLite stores booleans as 0/1 INTEGER.
    Bool,
}

impl Carry {
    fn for_pg_type(table: &str, column: &str, data_type: &str) -> Result<Self, StoreError> {
        Ok(match data_type {
            "bigint" | "integer" | "smallint" => Self::Int,
            "real" | "double precision" => Self::Float,
            "text" | "character varying" => Self::Text,
            "bytea" => Self::Bytes,
            "boolean" => Self::Bool,
            other => {
                return Err(StoreError::Parse(format!(
                    "migration cannot carry {table}.{column}: postgres type {other}"
                )))
            }
        })
    }
}

/// The copied columns of `table`: every non-generated SQLite column, with
/// how each is carried into its postgres column.
async fn columns(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
) -> Result<Vec<(String, Carry)>, StoreError> {
    // `pragma_table_info` leaves out generated columns, which the target
    // computes itself.
    let sqlite_cols: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .bind(table)
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let pg_cols: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_generated::text \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await
    .map_err(StoreError::Sqlx)?;
    if pg_cols.is_empty() {
        return Err(StoreError::Parse(format!(
            "migration target has no table {table}"
        )));
    }
    let mut out = Vec::with_capacity(sqlite_cols.len());
    for (name,) in sqlite_cols {
        let Some((_, data_type, generated)) = pg_cols.iter().find(|(c, ..)| *c == name) else {
            return Err(StoreError::Parse(format!(
                "migration target has no column {table}.{name}"
            )));
        };
        if generated == "ALWAYS" {
            continue;
        }
        out.push((name.clone(), Carry::for_pg_type(table, &name, data_type)?));
    }
    Ok(out)
}

/// One value on its way across, already in its postgres shape.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Int(Option<i64>),
    Float(Option<f64>),
    Text(Option<String>),
    Bytes(Option<Vec<u8>>),
    Bool(Option<bool>),
}

fn read(row: &sqlx::sqlite::SqliteRow, i: usize, carry: Carry) -> Result<Value, StoreError> {
    // Unchecked: a column without STRICT may hold another storage class
    // (an INTEGER in a REAL column), which SQLite converts on read.
    Ok(match carry {
        Carry::Int => Value::Int(row.try_get_unchecked(i).map_err(StoreError::Sqlx)?),
        Carry::Float => Value::Float(row.try_get_unchecked(i).map_err(StoreError::Sqlx)?),
        Carry::Text => Value::Text(row.try_get_unchecked(i).map_err(StoreError::Sqlx)?),
        Carry::Bytes => Value::Bytes(row.try_get_unchecked(i).map_err(StoreError::Sqlx)?),
        Carry::Bool => Value::Bool(
            row.try_get_unchecked::<Option<i64>, _>(i)
                .map_err(StoreError::Sqlx)?
                .map(|v| v != 0),
        ),
    })
}

/// Rows the store's background backfill has not reached yet have no sort
/// name; postgres gets one derived from the title, as the backfill would.
fn fill_sort_name(table: &str, cols: &[(String, Carry)], values: &mut [Value]) {
    if table != "media_items" {
        return;
    }
    let at = |name: &str| cols.iter().position(|(c, _)| c == name);
    let (Some(sort), Some(title)) = (at("sort_name"), at("title")) else {
        return;
    };
    if let (Value::Text(None), Value::Text(Some(t))) = (&values[sort], &values[title]) {
        values[sort] = Value::Text(Some(pharos_core::sort_name(t)));
    }
}

fn quoted(cols: &[(String, Carry)]) -> String {
    cols.iter()
        .map(|(c, _)| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Copy every row of `table`, bound one row at a time inside the one
/// transaction so the statement is prepared once and reused.
async fn copy_table(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
) -> Result<u64, StoreError> {
    let cols = columns(sp, tx, table).await?;
    let list = quoted(&cols);
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {list} FROM \"{table}\""
    )))
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let placeholders = (1..=cols.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Explicit ids go into GENERATED ALWAYS identity columns too; the clause
    // is a no-op on a table without one.
    let insert =
        format!("INSERT INTO \"{table}\" ({list}) OVERRIDING SYSTEM VALUE VALUES ({placeholders})");
    let n = rows.len() as u64;
    for row in rows {
        let mut values = cols
            .iter()
            .enumerate()
            .map(|(i, (_, carry))| read(&row, i, *carry))
            .collect::<Result<Vec<_>, _>>()?;
        fill_sort_name(table, &cols, &mut values);
        let mut q = sqlx::query(sqlx::AssertSqlSafe(insert.as_str()));
        for v in values {
            q = match v {
                Value::Int(v) => q.bind(v),
                Value::Float(v) => q.bind(v),
                Value::Text(v) => q.bind(v),
                Value::Bytes(v) => q.bind(v),
                Value::Bool(v) => q.bind(v),
            };
        }
        q.execute(&mut **tx)
            .await
            .map_err(|e| StoreError::Parse(format!("migration insert into {table} failed: {e}")))?;
    }
    if cols.iter().any(|(c, _)| c == "id") {
        setval(tx, table).await?;
    }
    Ok(n)
}

/// Move `table`'s id sequence past the copied max id. Identity and serial
/// columns alike: `pg_get_serial_sequence` resolves both. An `id` with no
/// sequence (a bytea user id, a fixed singleton row) is left alone.
async fn setval(tx: &mut Transaction<'_, Postgres>, table: &str) -> Result<(), StoreError> {
    let (seq,): (Option<String>,) = sqlx::query_as("SELECT pg_get_serial_sequence($1, 'id')")
        .bind(table)
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    if seq.is_none() {
        return Ok(());
    }
    let sql = format!(
        "SELECT setval(pg_get_serial_sequence('{table}', 'id'), \
         COALESCE((SELECT MAX(id) FROM {table}), 1))"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    Ok(())
}

/// Independent post-commit check: every copied table's sqlite row count
/// must equal its postgres row count. Named per-table so a mismatch
/// pinpoints exactly which table under/over-copied.
async fn verify_counts(
    sp: &SqlitePool,
    dp: &PgPool,
    tables: &[(String, u64)],
) -> Result<(), StoreError> {
    for (table, _) in tables {
        let (sqlite_count,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(sp)
                .await
                .map_err(StoreError::Sqlx)?;
        let (pg_count,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(dp)
                .await
                .map_err(StoreError::Sqlx)?;
        if sqlite_count != pg_count {
            return Err(StoreError::Parse(format!(
                "migration count mismatch on {table}: sqlite={sqlite_count} postgres={pg_count}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// B1071: a table added to the SQLite schema must be put on one list or
    /// the other, or the migration refuses to run. Runs without postgres.
    #[tokio::test]
    async fn every_sqlite_table_is_copied_or_left_with_a_reason() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        assert_eq!(
            unlisted_tables(store.pool()).await.unwrap(),
            Vec::<String>::new(),
            "add each to COPIED_TABLES or NOT_COPIED"
        );
        for table in COPIED_TABLES {
            assert!(
                !NOT_COPIED.iter().any(|(n, _)| n == table),
                "{table} is on both lists"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for table in COPIED_TABLES {
            assert!(seen.insert(table), "{table} is copied twice");
        }
    }

    #[tokio::test]
    async fn an_unlisted_table_stops_the_migration_before_any_write() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE brand_new (id INTEGER PRIMARY KEY)")
            .execute(store.pool())
            .await
            .unwrap();
        let err = refuse_unknown_tables(store.pool()).await.unwrap_err();
        assert!(err.to_string().contains("brand_new"), "{err}");
    }
}
