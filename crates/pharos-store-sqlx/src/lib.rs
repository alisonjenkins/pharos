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

/// T-fix-RC1 — single-row mutable branding/config snapshot persisted
/// in the `runtime_config` table by both sqlite + postgres backends.
/// None for any field means "no override; fall back to config.toml".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub server_name: Option<String>,
    pub login_disclaimer: Option<String>,
    pub custom_css: Option<String>,
}

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
