//! Server-wide config / identity persistence shared by both backends.
//!
//! These operations don't belong to any `pharos-core` domain trait (they
//! read/write server-local state — identity UUID, branding overrides,
//! named config blobs, library renames, and the LIB-B2 "distinct probe
//! field" lookups used to resolve synthetic ParentId pivots) so the trait
//! lives here, next to `StoreError`/`RuntimeConfig`, rather than in
//! `pharos-core`.

use crate::{RuntimeConfig, StoreError};

pub trait ServerConfigStore: Send + Sync {
    /// Read or initialise this server's stable identity UUID. First call
    /// in a fresh install writes a new row; subsequent calls return the
    /// same value.
    fn load_or_create_server_id(
        &self,
    ) -> impl std::future::Future<Output = Result<String, StoreError>> + Send;

    /// Read the persisted runtime config snapshot. Returns `Default` when
    /// the row has never been written.
    fn load_runtime_config(
        &self,
    ) -> impl std::future::Future<Output = Result<RuntimeConfig, StoreError>> + Send;

    /// Upsert the runtime config snapshot wholesale.
    fn set_runtime_config(
        &self,
        cfg: &RuntimeConfig,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// T69 — rename a library (identified by its `wire_id`) in place.
    /// Returns the number of rows updated (0 = no library with that wire
    /// id).
    fn rename_library(
        &self,
        wire_id: &str,
        new_name: &str,
    ) -> impl std::future::Future<Output = Result<u64, StoreError>> + Send;

    /// T72 — read a persisted named-configuration section blob by key.
    /// Returns the raw JSON string, or `None` when the section has never
    /// been written.
    fn load_named_config(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, StoreError>> + Send;

    /// T72 — upsert a named-configuration section blob.
    fn set_named_config(
        &self,
        key: &str,
        value: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// LIB-B2 — distinct `(series_folder, series_name)` keys, for resolving
    /// a `?ParentId=<series synth id>` to its folder/name.
    fn distinct_series_keys(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<(Option<String>, String)>, StoreError>> + Send;

    /// LIB-B2 — distinct `(series_folder, series_name, season_number)`
    /// keys, for resolving a `?ParentId=<season synth id>`.
    fn distinct_season_keys(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<(Option<String>, String, i64)>, StoreError>> + Send;

    /// LIB-B2 — distinct non-empty artist + album_artist names, for
    /// resolving a `?ParentId=<artist synth id>`.
    fn distinct_artist_names(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// LIB-B2 — distinct non-empty album names, for resolving a
    /// `?ParentId=<album synth id>`.
    fn distinct_album_names(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// LIB-B2 — distinct raw `genre` probe strings, for the legacy
    /// `?ParentId=<genre synth id>` fallback.
    fn distinct_genre_fields(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
}
