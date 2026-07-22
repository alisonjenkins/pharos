//! Provider-agnostic online metadata/artwork enrichment abstraction (T5 of
//! the online-metadata-enrichment feature). This module defines the shape
//! every concrete provider (TMDB, TVDB, ...) fills in — [`EnrichedMetadata`]
//! is the provider-neutral output, [`OnlineEnricher`] is the trait the
//! later orchestrator (T9) drives generically, and merge (T7) reconciles
//! multiple providers' [`EnrichedMetadata`] for the same item.

use std::future::Future;

use pharos_core::{ArtworkRole, MediaKind, PersonRef, SearchCandidate};

/// One piece of remote artwork a provider can offer: its role (Primary /
/// Backdrop / Thumb / ...) and the fully-qualified URL to fetch the bytes
/// from. Downloading is deferred to [`OnlineEnricher::fetch_image_bytes`]
/// (T8 wires the actual cache-write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteArt {
    pub role: ArtworkRole,
    pub url: String,
}

/// Provider-neutral metadata pulled from a single online provider for a
/// single item. Every field is optional/empty by default so a provider
/// that only has partial data (e.g. TVDB with no rating) can still return
/// a useful `EnrichedMetadata` — merge (T7) reconciles multiple providers'
/// results field-by-field rather than requiring full coverage from any one.
#[derive(Debug, Clone, Default)]
pub struct EnrichedMetadata {
    pub title: Option<String>,
    pub overview: Option<String>,
    pub tagline: Option<String>,
    pub production_year: Option<u32>,
    pub premiere_date: Option<i64>,
    pub community_rating: Option<f32>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub people: Vec<PersonRef>,
    /// The matched id on THIS provider (e.g. the TMDB movie id).
    pub provider_id: Option<String>,
    /// Cross-provider bridge (e.g. a TVDB result's TMDB id) so a provider
    /// with no image CDN of its own (TVDB gap-fill) can still hand off to
    /// TMDB's artwork for the same title.
    pub also_tmdb_id: Option<String>,
    pub artwork: Vec<RemoteArt>,
}

/// A single online metadata/artwork provider (TMDB, TVDB, ...). The
/// orchestrator (T9) drives this generically: search to resolve a
/// candidate id, then fetch the full record for that id. Implementations
/// must never panic on malformed provider responses — return `None` /
/// empty `Vec` and let the caller degrade gracefully (a provider blip must
/// never fail a scan).
pub trait OnlineEnricher: Send + Sync {
    /// Stable provider token, e.g. `"tmdb"` / `"tvdb"`.
    fn provider(&self) -> &'static str;

    /// Whether this provider has anything useful to offer for `kind`
    /// (movies vs. TV vs. audio).
    fn supports(&self, kind: MediaKind) -> bool;

    /// Search the provider for `title` (optionally narrowed by `year`),
    /// returning ranked candidates for [`pharos_core::match_best`] to pick
    /// from.
    fn search(
        &self,
        kind: MediaKind,
        title: &str,
        year: Option<u32>,
    ) -> impl Future<Output = Vec<SearchCandidate>> + Send;

    /// Fetch the full record for a provider `id` already resolved (via
    /// [`search`](Self::search) + `match_best`, or a stored provider id).
    /// `season`/`episode` are set together when fetching a single episode
    /// under a TV series id; both `None` fetches the movie/series-level
    /// record.
    fn fetch(
        &self,
        kind: MediaKind,
        id: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> impl Future<Output = Option<EnrichedMetadata>> + Send;

    /// Download the raw bytes of an artwork URL from
    /// [`RemoteArt::url`](RemoteArt) (or [`EnrichedMetadata::also_tmdb_id`]
    /// bridged art). `None` on any transport/HTTP error.
    fn fetch_image_bytes(&self, url: &str) -> impl Future<Output = Option<Vec<u8>>> + Send;
}
