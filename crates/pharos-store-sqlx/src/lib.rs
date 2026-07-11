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

#[cfg(feature = "sqlite")]
mod transcode_session_sqlite;

#[cfg(feature = "sqlite")]
mod sync_group_sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub mod pg_sync_bus;

#[cfg(feature = "postgres")]
pub use pg_sync_bus::PgSyncBus;

#[cfg(all(feature = "sqlite", feature = "postgres"))]
pub mod any;

#[cfg(all(feature = "sqlite", feature = "postgres"))]
pub mod migrate;

// Server-wide config / identity persistence trait, shared by both
// backends (not a pharos-core domain trait — see module docs).
pub mod server_config;
pub use server_config::ServerConfigStore;

// Singleton background-work leadership (advisory lock under Postgres).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub mod bg_lock;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use bg_lock::BgLeadership;

// JSON adapter for MediaProbe.subtitle_tracks persistence — kept
// outside the feature gates so both backends use it.
pub mod attachment_json;
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

// Session-token hashing + lifetime, shared by both backends' TokenStore.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) mod auth_token {
    use sha2::{Digest, Sha256};

    /// Default session-token lifetime: 90 days. Tokens are stored hashed
    /// with `created_at + this` as `expires_at`; `resolve` rejects any
    /// token past its expiry. Long enough that TV / mobile clients aren't
    /// forced to re-log frequently, short enough to bound a leaked token.
    pub(crate) const TTL_SECS: i64 = 90 * 24 * 60 * 60;

    /// Hash a raw session token for at-rest storage / lookup. Tokens are
    /// 122-bit random UUIDv4s, so a single SHA-256 (not a slow password
    /// KDF) is sufficient — there is nothing to brute-force. Returns
    /// lowercase hex.
    pub(crate) fn hash(token: &str) -> String {
        let digest = Sha256::digest(token.as_bytes());
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hash_is_stable_hex_and_not_the_input() {
            let h = hash("abc123");
            assert_eq!(h.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
            assert_ne!(h, "abc123");
            assert_eq!(h, hash("abc123")); // deterministic
            assert_ne!(h, hash("abc124")); // sensitive to input
        }
    }
}

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
