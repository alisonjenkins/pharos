//! Postgres backend — wiring stub. Real impl tracked separately.
//! Compiles under `--features postgres` so the feature gate stays exercised.

use crate::StoreError;
use pharos_core::{DomainError, DomainResult, MediaId, MediaItem, MediaStore};
use sqlx::PgPool;

#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl MediaStore for PostgresStore {
    async fn get(&self, _id: MediaId) -> DomainResult<MediaItem> {
        Err(DomainError::Backend(
            "postgres backend not yet implemented".into(),
        ))
    }
    async fn put(&self, _item: MediaItem) -> DomainResult<()> {
        Err(DomainError::Backend(
            "postgres backend not yet implemented".into(),
        ))
    }
    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        Err(DomainError::Backend(
            "postgres backend not yet implemented".into(),
        ))
    }
}
