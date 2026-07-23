//! Shared row shape for the `series_metadata` table (T9-series). One
//! `#[derive(sqlx::FromRow)]` struct decodes from BOTH the sqlite and postgres
//! backends — the column types (TEXT / REAL / INTEGER|BIGINT) map identically —
//! so the two `SeriesMetadataStore` impls share one decode path.

use pharos_core::SeriesMetadata;

/// Column list for `SELECT`s, in the [`SeriesMetaRow`] field order.
pub(crate) const SERIES_META_COLUMNS: &str = "series_key, series_name, match_provider, \
    match_external_id, match_source, match_confidence, metadata_refreshed_at, \
    overview, community_rating, premiere_date, official_rating, genres, studios, \
    provider_ids, poster_locator, backdrop_locator";

#[derive(sqlx::FromRow)]
pub(crate) struct SeriesMetaRow {
    pub series_key: String,
    pub series_name: String,
    pub match_provider: Option<String>,
    pub match_external_id: Option<String>,
    pub match_source: Option<String>,
    pub match_confidence: Option<f32>,
    pub metadata_refreshed_at: Option<i64>,
    pub overview: Option<String>,
    pub community_rating: Option<f32>,
    pub premiere_date: Option<i64>,
    pub official_rating: Option<String>,
    pub genres: Option<String>,
    pub studios: Option<String>,
    pub provider_ids: Option<String>,
    pub poster_locator: Option<String>,
    pub backdrop_locator: Option<String>,
}

impl From<SeriesMetaRow> for SeriesMetadata {
    fn from(r: SeriesMetaRow) -> Self {
        SeriesMetadata {
            series_key: r.series_key,
            series_name: r.series_name,
            match_provider: r.match_provider,
            match_external_id: r.match_external_id,
            match_source: r.match_source,
            match_confidence: r.match_confidence,
            metadata_refreshed_at: r.metadata_refreshed_at,
            overview: r.overview,
            community_rating: r.community_rating,
            premiere_date: r.premiere_date,
            official_rating: r.official_rating,
            genres: crate::string_list_json::decode(r.genres.as_deref()),
            studios: crate::string_list_json::decode(r.studios.as_deref()),
            provider_ids: crate::provider_ids_json::decode(r.provider_ids.as_deref()),
            poster_locator: r.poster_locator,
            backdrop_locator: r.backdrop_locator,
        }
    }
}
