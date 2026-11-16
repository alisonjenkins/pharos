//! `SubtitleCheckStore` for both backends (spec 040, migration 0081): one row
//! per item and subtitle track source; see [`pharos_core::SubtitleCheckRow`].

use pharos_core::{DomainError, DomainResult, MediaId, SubtitleCheckRow};

const PUT: &str = "INSERT INTO subtitle_check \
       (item_id, source_key, source_mtime, source_size, sidecar_mtime, sidecar_size, \
        check_version, verdict, reason, offset_ms, ratio, scores_json, attempts, retry_at, \
        checked_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
     ON CONFLICT (item_id, source_key) DO UPDATE SET \
       source_mtime = excluded.source_mtime, source_size = excluded.source_size, \
       sidecar_mtime = excluded.sidecar_mtime, sidecar_size = excluded.sidecar_size, \
       check_version = excluded.check_version, verdict = excluded.verdict, \
       reason = excluded.reason, offset_ms = excluded.offset_ms, ratio = excluded.ratio, \
       scores_json = excluded.scores_json, attempts = excluded.attempts, \
       retry_at = excluded.retry_at, checked_at = excluded.checked_at";

const GET: &str = "SELECT item_id, source_key, source_mtime, source_size, sidecar_mtime, \
     sidecar_size, check_version, verdict, reason, offset_ms, ratio, scores_json, attempts, \
     retry_at, checked_at FROM subtitle_check WHERE item_id = $1 ORDER BY source_key";

/// `scores_json` is selected as NULL: the scores are the bulk of the data.
const INDEX: &str = "SELECT item_id, source_key, source_mtime, source_size, sidecar_mtime, \
     sidecar_size, check_version, verdict, reason, offset_ms, ratio, CAST(NULL AS TEXT) AS scores_json, \
     attempts, retry_at, checked_at FROM subtitle_check ORDER BY item_id, source_key";

#[derive(sqlx::FromRow)]
struct Row {
    item_id: i64,
    source_key: String,
    source_mtime: i64,
    source_size: i64,
    sidecar_mtime: Option<i64>,
    sidecar_size: Option<i64>,
    check_version: i64,
    verdict: Option<String>,
    reason: String,
    offset_ms: Option<i64>,
    ratio: Option<f64>,
    scores_json: Option<String>,
    attempts: i64,
    retry_at: Option<i64>,
    checked_at: i64,
}

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn conv<T, U: TryFrom<T>>(v: T, what: &str) -> DomainResult<U>
where
    U::Error: std::fmt::Display,
{
    U::try_from(v).map_err(|e| DomainError::Backend(format!("subtitle_check {what}: {e}")))
}

fn from_row(r: Row) -> DomainResult<SubtitleCheckRow> {
    Ok(SubtitleCheckRow {
        item_id: conv(r.item_id, "item_id")?,
        source_key: r.source_key,
        source_mtime: r.source_mtime,
        source_size: conv(r.source_size, "source_size")?,
        sidecar_mtime: r.sidecar_mtime,
        sidecar_size: r
            .sidecar_size
            .map(|v| conv(v, "sidecar_size"))
            .transpose()?,
        check_version: conv(r.check_version, "check_version")?,
        verdict: r.verdict,
        reason: r.reason,
        offset_ms: r.offset_ms,
        ratio: r.ratio,
        scores_json: r.scores_json,
        attempts: conv(r.attempts, "attempts")?,
        retry_at: r.retry_at,
        checked_at: r.checked_at,
    })
}

macro_rules! impl_store {
    ($store:ty) => {
        impl pharos_core::SubtitleCheckStore for $store {
            async fn put_subtitle_check(&self, row: SubtitleCheckRow) -> DomainResult<()> {
                sqlx::query(PUT)
                    .bind(conv::<u64, i64>(row.item_id, "item_id")?)
                    .bind(row.source_key)
                    .bind(row.source_mtime)
                    .bind(conv::<u64, i64>(row.source_size, "source_size")?)
                    .bind(row.sidecar_mtime)
                    .bind(
                        row.sidecar_size
                            .map(|v| conv::<u64, i64>(v, "sidecar_size"))
                            .transpose()?,
                    )
                    .bind(i64::from(row.check_version))
                    .bind(row.verdict)
                    .bind(row.reason)
                    .bind(row.offset_ms)
                    .bind(row.ratio)
                    .bind(row.scores_json)
                    .bind(i64::from(row.attempts))
                    .bind(row.retry_at)
                    .bind(row.checked_at)
                    .execute(self.pool())
                    .await
                    .map_err(backend)?;
                Ok(())
            }

            async fn subtitle_checks(&self, item: MediaId) -> DomainResult<Vec<SubtitleCheckRow>> {
                let rows: Vec<Row> = sqlx::query_as(GET)
                    .bind(conv::<u64, i64>(item, "item_id")?)
                    .fetch_all(self.pool())
                    .await
                    .map_err(backend)?;
                rows.into_iter().map(from_row).collect()
            }

            async fn subtitle_check_index(&self) -> DomainResult<Vec<SubtitleCheckRow>> {
                let rows: Vec<Row> = sqlx::query_as(INDEX)
                    .fetch_all(self.pool())
                    .await
                    .map_err(backend)?;
                rows.into_iter().map(from_row).collect()
            }
        }
    };
}

#[cfg(feature = "sqlite")]
impl_store!(crate::sqlite::SqliteStore);

#[cfg(feature = "postgres")]
impl_store!(crate::postgres::PostgresStore);
