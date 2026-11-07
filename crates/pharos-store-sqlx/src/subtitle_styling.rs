//! `SubtitleStylingStore` for both backends (spec 028, migration 0068). The
//! row is the style director's plan for one track of an item; see
//! [`pharos_core::SubtitleStyling`] for what the signature columns mean.

use pharos_core::{DomainError, DomainResult, MediaId, SubtitleStyling};

const PUT: &str = "INSERT INTO subtitle_styling \
       (item_id, source_index, source_mtime, source_size, sidecar_mtime, sidecar_size, \
        director_version, outcome, plan_json, styled_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (item_id, source_index) DO UPDATE SET \
       source_mtime = excluded.source_mtime, source_size = excluded.source_size, \
       sidecar_mtime = excluded.sidecar_mtime, sidecar_size = excluded.sidecar_size, \
       director_version = excluded.director_version, outcome = excluded.outcome, \
       plan_json = excluded.plan_json, styled_at = excluded.styled_at";

const DELETE_OTHERS: &str =
    "DELETE FROM subtitle_styling WHERE item_id = $1 AND source_index <> $2";

const GET: &str = "SELECT item_id, source_index, source_mtime, source_size, sidecar_mtime, \
     sidecar_size, director_version, outcome, plan_json, styled_at \
     FROM subtitle_styling WHERE item_id = $1";

/// `plan_json` is selected as NULL: the plans are the bulk of the data.
const INDEX: &str = "SELECT item_id, source_index, source_mtime, source_size, sidecar_mtime, \
     sidecar_size, director_version, outcome, CAST(NULL AS TEXT), styled_at \
     FROM subtitle_styling ORDER BY item_id, source_index";

type Row = (
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
    String,
    Option<String>,
    i64,
);

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn conv<T, U: TryFrom<T>>(v: T, what: &str) -> DomainResult<U>
where
    U::Error: std::fmt::Display,
{
    U::try_from(v).map_err(|e| DomainError::Backend(format!("subtitle_styling {what}: {e}")))
}

fn from_row(r: Row) -> DomainResult<SubtitleStyling> {
    Ok(SubtitleStyling {
        item_id: conv(r.0, "item_id")?,
        source_index: conv(r.1, "source_index")?,
        source_mtime: r.2,
        source_size: conv(r.3, "source_size")?,
        sidecar_mtime: r.4,
        sidecar_size: r.5.map(|v| conv(v, "sidecar_size")).transpose()?,
        director_version: conv(r.6, "director_version")?,
        outcome: r.7,
        plan_json: r.8,
        styled_at: r.9,
    })
}

fn item_key(item: MediaId) -> DomainResult<i64> {
    conv(item, "item_id")
}

macro_rules! impl_store {
    ($store:ty) => {
        impl pharos_core::SubtitleStylingStore for $store {
            async fn put_subtitle_styling(&self, row: SubtitleStyling) -> DomainResult<()> {
                let item = item_key(row.item_id)?;
                let index: i64 = i64::from(row.source_index);
                let size: i64 = conv(row.source_size, "source_size")?;
                let sidecar_size: Option<i64> = row
                    .sidecar_size
                    .map(|v| conv(v, "sidecar_size"))
                    .transpose()?;
                let mut tx = self.pool().begin().await.map_err(backend)?;
                sqlx::query(PUT)
                    .bind(item)
                    .bind(index)
                    .bind(row.source_mtime)
                    .bind(size)
                    .bind(row.sidecar_mtime)
                    .bind(sidecar_size)
                    .bind(i64::from(row.director_version))
                    .bind(row.outcome)
                    .bind(row.plan_json)
                    .bind(row.styled_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                sqlx::query(DELETE_OTHERS)
                    .bind(item)
                    .bind(index)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                tx.commit().await.map_err(backend)
            }

            async fn subtitle_styling(
                &self,
                item: MediaId,
            ) -> DomainResult<Option<SubtitleStyling>> {
                let row: Option<Row> = sqlx::query_as(GET)
                    .bind(item_key(item)?)
                    .fetch_optional(self.pool())
                    .await
                    .map_err(backend)?;
                row.map(from_row).transpose()
            }

            async fn subtitle_styling_index(&self) -> DomainResult<Vec<SubtitleStyling>> {
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
