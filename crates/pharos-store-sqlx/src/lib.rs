//! sqlx-backed `MediaStore` impls. Features select backend:
//! - `sqlite` (default): in-process SQLite via `sqlx::Sqlite`.
//! - `postgres`: server-side Postgres via `sqlx::Postgres` (stub — full impl pending).

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("parse: {0}")]
    Parse(String),
}

impl From<StoreError> for pharos_core::DomainError {
    fn from(e: StoreError) -> Self {
        pharos_core::DomainError::Backend(e.to_string())
    }
}
