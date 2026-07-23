//! Provider-agnostic online metadata/artwork enrichment abstraction (T5 of
//! the online-metadata-enrichment feature). This module defines the shape
//! every concrete provider (TMDB, TVDB, ...) fills in — [`EnrichedMetadata`]
//! is the provider-neutral output, [`OnlineEnricher`] is the trait the
//! later orchestrator (T9) drives generically, and merge (T7) reconciles
//! multiple providers' [`EnrichedMetadata`] for the same item.

use std::future::Future;

use pharos_cache::image_cache::{ImageCache, ImageRole};
use pharos_core::{
    ArtworkRole, DomainError, DomainResult, MediaItem, MediaKind, MediaStore, PersonRef,
    SearchCandidate,
};

/// One piece of remote artwork a provider can offer: its role (Primary /
/// Backdrop / Thumb / ...) and the fully-qualified URL to fetch the bytes
/// from. Downloading is deferred to [`OnlineEnricher::fetch_image_bytes`]
/// (T8 wires the actual cache-write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteArt {
    pub role: ArtworkRole,
    pub url: String,
}

/// One candidate image a provider offers for an already-resolved id, richer
/// than [`RemoteArt`] (carries dimensions / language / rating so the
/// Edit-Images picker can show and sort them). Downloading is still deferred
/// to [`OnlineEnricher::fetch_image_bytes`] on the chosen [`RemoteImage::url`].
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteImage {
    pub role: ArtworkRole,
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub language: Option<String>,
    pub community_rating: Option<f32>,
    pub vote_count: Option<u32>,
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

    /// All candidate images the provider offers for an already-resolved `id`
    /// (every role in one call). Empty `Vec` on any transport/HTTP/decode
    /// error — best-effort, never panics, never fails the caller.
    fn list_images(
        &self,
        kind: MediaKind,
        id: &str,
    ) -> impl Future<Output = Vec<RemoteImage>> + Send;
}

/// Sets `slot` only when it is currently unset — local data always wins
/// over online enrichment.
fn fill<T>(slot: &mut Option<T>, v: Option<T>) {
    if slot.is_none() {
        if let Some(v) = v {
            *slot = Some(v);
        }
    }
}

/// Join-entity rows (genres/people) [`apply_enrichment`] decided should be
/// linked to the item — only non-empty when the item had none of that kind
/// already (fill-if-empty; see [`apply_enrichment`]).
pub struct AppliedEnrichment {
    pub genres: Vec<String>,
    pub people: Vec<PersonRef>,
}

/// Folds one provider's [`EnrichedMetadata`] onto a stored `item` WITHOUT
/// overriding curated local data: scalars fill only when the item's field
/// is `None` (local always wins), and join entities (genres/people) are
/// handed back for linking ONLY when `counts` shows the item currently has
/// none of that kind (fill-if-empty) — a curated NFO genre list is never
/// diluted by an online guess.
///
/// Deliberately does not touch `title` (local title stays authoritative)
/// or `metadata.provider_ids` (the orchestrator sets those once it knows
/// which provider matched — T9).
pub fn apply_enrichment(
    item: &mut pharos_core::MediaItem,
    counts: pharos_core::EntityCounts,
    e: &EnrichedMetadata,
) -> AppliedEnrichment {
    let md = &mut item.metadata;
    fill(&mut md.overview, e.overview.clone());
    fill(&mut md.tagline, e.tagline.clone());
    fill(&mut md.production_year, e.production_year);
    fill(&mut md.premiere_date, e.premiere_date);
    fill(&mut md.community_rating, e.community_rating);
    fill(&mut md.official_rating, e.official_rating.clone());
    AppliedEnrichment {
        genres: if counts.genres == 0 {
            e.genres.clone()
        } else {
            vec![]
        },
        people: if counts.people == 0 {
            e.people.clone()
        } else {
            vec![]
        },
    }
}

/// The [`pharos_cache::image_cache::ImageRole`] that corresponds to an
/// [`ArtworkRole`] (T8). The two enums share variant names by design but
/// live in different crates (`pharos-core` has no cache-layer dependency),
/// so a provider's remote-art role can't be used directly as a cache-write
/// argument. Match is exhaustive so a new `ArtworkRole` variant fails to
/// compile here rather than silently losing its cache role.
fn to_cache_role(role: ArtworkRole) -> ImageRole {
    match role {
        ArtworkRole::Primary => ImageRole::Primary,
        ArtworkRole::Backdrop => ImageRole::Backdrop,
        ArtworkRole::Thumb => ImageRole::Thumb,
        ArtworkRole::Logo => ImageRole::Logo,
        ArtworkRole::Banner => ImageRole::Banner,
        ArtworkRole::Disc => ImageRole::Disc,
        ArtworkRole::Art => ImageRole::Art,
    }
}

/// T8 — persist already-downloaded provider artwork bytes: write them into
/// the on-disk image cache (same tree the local-sidecar / upload paths use),
/// then record the resulting cache-file path in the `artwork` table under
/// `provider` as `source` (`"tmdb"`/`"tvdb"`). Once recorded, the widened
/// `has_primary_art` predicate and `local_artwork_path` filter (this task)
/// serve it identically to a local sidecar — the download step is the only
/// thing distinguishing it from local art. The orchestrator (T9) calls this
/// once per fetched [`RemoteArt`] after downloading `bytes` via
/// [`OnlineEnricher::fetch_image_bytes`](OnlineEnricher::fetch_image_bytes).
pub async fn download_and_cache_art<S: MediaStore>(
    cache: &ImageCache,
    store: &S,
    item: &MediaItem,
    provider: &str,
    art: &RemoteArt,
    bytes: Vec<u8>,
) -> DomainResult<()> {
    let role = to_cache_role(art.role);
    let path = cache
        .upload(item.id, role, item.kind, 0, &bytes)
        .await
        .map_err(|e| DomainError::Backend(format!("artwork cache upload: {e}")))?;
    store
        .set_artwork(
            item.id,
            art.role.as_str(),
            provider,
            &path.to_string_lossy(),
        )
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A `MediaItem` with all metadata fields `None`/empty — the baseline
    /// for fill-if-absent assertions.
    fn bare_movie() -> pharos_core::MediaItem {
        pharos_core::MediaItem {
            kind: MediaKind::Movie,
            ..Default::default()
        }
    }

    #[test]
    fn apply_enrichment_fills_only_missing_scalars() {
        let mut item = bare_movie(); // helper: all metadata None
        item.metadata.overview = Some("local overview".into());
        let e = EnrichedMetadata {
            overview: Some("online overview".into()),
            production_year: Some(1999),
            genres: vec!["Sci-Fi".into()],
            provider_id: Some("603".into()),
            ..EnrichedMetadata::default()
        };
        let applied = apply_enrichment(&mut item, pharos_core::EntityCounts::default(), &e);
        assert_eq!(item.metadata.overview.as_deref(), Some("local overview")); // local kept
        assert_eq!(item.metadata.production_year, Some(1999)); // gap filled
        assert_eq!(applied.genres, vec!["Sci-Fi"]); // item had 0 genres
    }

    #[test]
    fn to_cache_role_maps_every_artwork_role() {
        // Exhaustive by construction (the match has no wildcard arm), but
        // pin the actual mapping so a future rename in either enum is
        // caught here rather than only at the call site.
        assert_eq!(to_cache_role(ArtworkRole::Primary), ImageRole::Primary);
        assert_eq!(to_cache_role(ArtworkRole::Backdrop), ImageRole::Backdrop);
        assert_eq!(to_cache_role(ArtworkRole::Thumb), ImageRole::Thumb);
        assert_eq!(to_cache_role(ArtworkRole::Logo), ImageRole::Logo);
        assert_eq!(to_cache_role(ArtworkRole::Banner), ImageRole::Banner);
        assert_eq!(to_cache_role(ArtworkRole::Disc), ImageRole::Disc);
        assert_eq!(to_cache_role(ArtworkRole::Art), ImageRole::Art);
    }

    #[tokio::test]
    async fn download_and_cache_art_writes_bytes_and_records_artwork_row() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path());
        let store = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
            .await
            .unwrap();
        let item = MediaItem {
            id: 900021,
            kind: MediaKind::Movie,
            title: "Arrival".into(),
            ..Default::default()
        };
        store.put(item.clone()).await.unwrap();
        let art = RemoteArt {
            role: ArtworkRole::Primary,
            url: "https://image.tmdb.org/t/p/w780/x.jpg".into(),
        };
        let bytes = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3];

        download_and_cache_art(&cache, &store, &item, "tmdb", &art, bytes.clone())
            .await
            .unwrap();

        // Bytes landed on disk under the cache tree.
        let expect_path =
            pharos_cache::image_cache::primary_path(td.path(), MediaKind::Movie, 900021);
        let on_disk = tokio::fs::read(&expect_path).await.unwrap();
        assert_eq!(on_disk, bytes);

        // The artwork row was recorded with `source = "tmdb"` and the
        // cache-file locator, which flips `has_primary_art`.
        let rows = store.artwork_for(900021).await.unwrap();
        let (role, source, locator) = rows
            .into_iter()
            .find(|(r, _, _)| r.eq_ignore_ascii_case("Primary"))
            .unwrap();
        assert_eq!(role, "Primary");
        assert_eq!(source, "tmdb");
        assert_eq!(locator, expect_path.to_string_lossy());
        assert!(store.get(900021).await.unwrap().has_primary_art);
    }

    #[test]
    fn apply_enrichment_skips_joins_when_already_populated() {
        let mut item = bare_movie();
        let e = EnrichedMetadata {
            genres: vec!["Sci-Fi".into()],
            ..EnrichedMetadata::default()
        };
        let counts = pharos_core::EntityCounts {
            genres: 3,
            people: 0,
            studios: 0,
        };
        let applied = apply_enrichment(&mut item, counts, &e);
        assert!(applied.genres.is_empty()); // local NFO genres present -> online genres not linked
    }
}
