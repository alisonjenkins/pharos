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

// JSON adapter for MediaProbe.audio_tracks (P16). Same pattern.
pub mod audio_track_json;

// JSON adapter for MediaMetadata.provider_ids (LIB-C9). Same pattern.
pub mod provider_ids_json;

// LIB-B1 — backend-agnostic SQL builder for MediaStore::query(). Shared by
// both backends (only the placeholder token + null-limit token differ).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) mod media_query;

/// LIB-A3 — build a root-scoped, path-boundary-safe SQL `LIKE` pattern for
/// the scan sweep. Matches only items strictly *under* `root` (i.e. paths
/// beginning `root` + path separator), never a sibling whose name merely
/// shares a string prefix (`/media/movies` must not match
/// `/media/movies-4k`). `%`/`_`/`\` in the root are escaped, so the query
/// MUST use `ESCAPE '\'`. The trailing `root/` is appended after escaping.
pub(crate) fn root_like_pattern(root: &str) -> String {
    let base = root.strip_suffix('/').unwrap_or(root);
    let mut out = String::with_capacity(base.len() + 2);
    for c in base.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("/%");
    out
}

#[cfg(test)]
mod root_like_tests {
    #![allow(clippy::unwrap_used)]
    use super::root_like_pattern;

    #[test]
    fn boundary_excludes_sibling_prefix() {
        // The classic bug: sweeping /media/movies must not match
        // /media/movies-4k. The pattern ends in "/%", so the separator is
        // mandatory.
        assert_eq!(root_like_pattern("/media/movies"), "/media/movies/%");
        assert_eq!(root_like_pattern("/media/movies/"), "/media/movies/%");
    }

    #[test]
    fn escapes_like_wildcards_in_root() {
        assert_eq!(root_like_pattern("/m_edia/50%off"), "/m\\_edia/50\\%off/%");
    }
}

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
