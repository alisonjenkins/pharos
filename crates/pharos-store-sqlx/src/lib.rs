//! sqlx-backed `MediaStore` impls. Features select backend:
//! - `sqlite` (default): in-process SQLite via `sqlx::Sqlite`.
//! - `postgres`: server-side Postgres via `sqlx::Postgres` (stub — full impl pending).

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
mod auth_sqlite;

#[cfg(feature = "sqlite")]
mod user_data_sqlite;

#[cfg(feature = "sqlite")]
mod preferences_sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

// JSON adapter for MediaProbe.subtitle_tracks persistence — kept
// outside the feature gates so both backends use it.
pub mod subtitle_track_json;

// JSON adapter for MediaProbe.chapters persistence (T54 phase 4
// / T57 phase 2). Same pattern as subtitle_tracks.
pub mod chapter_json;

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
