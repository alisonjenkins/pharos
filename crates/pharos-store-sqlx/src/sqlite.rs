use crate::StoreError;
use pharos_core::{DomainError, DomainResult, MediaId, MediaItem, MediaKind, MediaStore};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open a pool against the given sqlx connect URL (e.g. `sqlite::memory:`,
    /// `sqlite:///var/lib/pharos/data.db`). Runs migrations to latest.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(StoreError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

impl MediaStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(media.id = %id))]
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        let id_i64 = i64::try_from(id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let row = sqlx::query_as::<_, MediaRow>(
            "SELECT id, path, title, kind FROM media_items WHERE id = ?",
        )
        .bind(id_i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        match row {
            Some(r) => r.into_domain(),
            None => Err(DomainError::NotFound(id)),
        }
    }

    #[tracing::instrument(skip(self, item), fields(media.id = %item.id, media.kind = item.kind.as_str()))]
    async fn put(&self, item: MediaItem) -> DomainResult<()> {
        let id_i64 = i64::try_from(item.id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let path = item
            .path
            .to_str()
            .ok_or_else(|| DomainError::Backend("non-utf8 path".into()))?;
        sqlx::query(
            "INSERT INTO media_items (id, path, title, kind) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET path = excluded.path,
                                           title = excluded.title,
                                           kind = excluded.kind",
        )
        .bind(id_i64)
        .bind(path)
        .bind(&item.title)
        .bind(item.kind.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        let rows = sqlx::query_as::<_, MediaRow>(
            "SELECT id, path, title, kind FROM media_items ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.into_iter().map(MediaRow::into_domain).collect()
    }
}

#[derive(sqlx::FromRow)]
struct MediaRow {
    id: i64,
    path: String,
    title: String,
    kind: String,
}

impl MediaRow {
    fn into_domain(self) -> DomainResult<MediaItem> {
        let id = u64::try_from(self.id)
            .map_err(|e| DomainError::Backend(format!("id negative: {e}")))?;
        let kind = MediaKind::from_str(&self.kind)?;
        Ok(MediaItem {
            id,
            path: self.path.into(),
            title: self.title,
            kind,
        })
    }
}
