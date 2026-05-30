//! LIB-D1 — merge-behaviour unit tests for [`MetadataResolver`].

use super::*;
use pharos_core::{
    ArtworkRef,
    ArtworkRole,
    ArtworkSource,
    DomainError,
    DomainResult,
    MediaKind,
    MediaProbe,
    MetadataProvider,
    MetadataRequest,
    MetadataResult,
    PersonKind,
    PersonRef,
    ProviderIds,
    // DomainResult used by the trait method signature in MockProvider.
};
use std::path::Path;

/// A canned provider: returns a fixed result, or a `Backend` error when
/// `fail` is set, for any request. (`DomainError` isn't `Clone`, so the
/// failure case is modelled as a flag rather than a stored `Result`.)
struct MockProvider {
    name: &'static str,
    priority: i32,
    supports: MediaKind,
    result: MetadataResult,
    fail: bool,
}

impl MockProvider {
    fn ok(name: &'static str, priority: i32, result: MetadataResult) -> Self {
        Self {
            name,
            priority,
            supports: MediaKind::Movie,
            result,
            fail: false,
        }
    }
}

impl MetadataProvider for MockProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    fn supports(&self, kind: MediaKind) -> bool {
        kind == self.supports
    }
    async fn fetch(&self, _req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        if self.fail {
            return Err(DomainError::Backend("boom".into()));
        }
        Ok(self.result.clone())
    }
}

fn request(kind: MediaKind, probe: &MediaProbe) -> MetadataRequest<'_> {
    MetadataRequest {
        path: Path::new("/media/movie.mkv"),
        kind,
        probe,
        series: None,
    }
}

#[tokio::test]
async fn higher_priority_wins_scalar_fields() {
    let probe = MediaProbe::default();
    let low = MockProvider::ok(
        "low",
        10,
        MetadataResult {
            title: Some("Low Title".into()),
            overview: Some("low overview".into()),
            community_rating: Some(5.0),
            production_year: Some(1999),
            provider_ids: ProviderIds {
                tmdb: Some("low-tmdb".into()),
                imdb: Some("tt-low".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let high = MockProvider::ok(
        "high",
        100,
        MetadataResult {
            title: Some("High Title".into()),
            overview: Some("high overview".into()),
            community_rating: Some(8.0),
            // production_year intentionally None — low's should fill it.
            provider_ids: ProviderIds {
                tmdb: Some("high-tmdb".into()),
                // imdb None — low's fills.
                ..Default::default()
            },
            ..Default::default()
        },
    );

    // Register low first to prove sort (not insertion order) drives merge.
    let resolver = MetadataResolver::new()
        .with_provider(low)
        .with_provider(high);
    assert_eq!(resolver.provider_count(), 2);

    let merged = resolver.resolve(&request(MediaKind::Movie, &probe)).await;

    // High priority wins the overlapping scalars.
    assert_eq!(merged.title.as_deref(), Some("High Title"));
    assert_eq!(merged.overview.as_deref(), Some("high overview"));
    assert_eq!(merged.community_rating, Some(8.0));
    assert_eq!(merged.provider_ids.tmdb.as_deref(), Some("high-tmdb"));
    // Fields high left None are filled from low.
    assert_eq!(merged.production_year, Some(1999));
    assert_eq!(merged.provider_ids.imdb.as_deref(), Some("tt-low"));
}

#[tokio::test]
async fn vec_fields_union_and_dedupe_in_priority_order() {
    let probe = MediaProbe::default();
    let high = MockProvider::ok(
        "high",
        100,
        MetadataResult {
            genres: vec!["Drama".into(), "Thriller".into()],
            studios: vec!["A24".into()],
            tags: vec!["4k".into()],
            people: vec![PersonRef {
                name: "Jane Doe".into(),
                kind: PersonKind::Director,
                ..Default::default()
            }],
            artwork: vec![ArtworkRef {
                role: ArtworkRole::Primary,
                source: ArtworkSource::LocalFile("/media/poster.jpg".into()),
            }],
            ..Default::default()
        },
    );
    let low = MockProvider::ok(
        "low",
        10,
        MetadataResult {
            // "Drama" overlaps -> deduped; "Crime" is new.
            genres: vec!["Drama".into(), "Crime".into()],
            studios: vec!["A24".into()], // overlap -> deduped
            tags: vec!["hdr".into()],
            people: vec![
                // Same person+kind+character -> deduped.
                PersonRef {
                    name: "Jane Doe".into(),
                    kind: PersonKind::Director,
                    ..Default::default()
                },
                // Distinct actor -> kept.
                PersonRef {
                    name: "John Roe".into(),
                    kind: PersonKind::Actor,
                    character: Some("Hero".into()),
                    ..Default::default()
                },
            ],
            artwork: vec![
                // Same role+source -> deduped.
                ArtworkRef {
                    role: ArtworkRole::Primary,
                    source: ArtworkSource::LocalFile("/media/poster.jpg".into()),
                },
                // Distinct backdrop -> kept.
                ArtworkRef {
                    role: ArtworkRole::Backdrop,
                    source: ArtworkSource::LocalFile("/media/fanart.jpg".into()),
                },
            ],
            ..Default::default()
        },
    );

    let resolver = MetadataResolver::new()
        .with_provider(low)
        .with_provider(high);
    let merged = resolver.resolve(&request(MediaKind::Movie, &probe)).await;

    // Priority order preserved (high first), overlaps deduped.
    assert_eq!(merged.genres, vec!["Drama", "Thriller", "Crime"]);
    assert_eq!(merged.studios, vec!["A24"]);
    assert_eq!(merged.tags, vec!["4k", "hdr"]);
    assert_eq!(merged.people.len(), 2);
    assert_eq!(merged.people[0].name, "Jane Doe");
    assert_eq!(merged.people[1].name, "John Roe");
    assert_eq!(merged.artwork.len(), 2);
    assert_eq!(merged.artwork[0].role, ArtworkRole::Primary);
    assert_eq!(merged.artwork[1].role, ArtworkRole::Backdrop);
}

#[tokio::test]
async fn err_provider_is_skipped_not_aborting() {
    let probe = MediaProbe::default();
    // Highest priority FAILS — must be skipped, lower provider still wins.
    let failing = MockProvider {
        name: "failing",
        priority: 1000,
        supports: MediaKind::Movie,
        result: MetadataResult::default(),
        fail: true,
    };
    let good = MockProvider::ok(
        "good",
        10,
        MetadataResult {
            overview: Some("survived".into()),
            genres: vec!["Sci-Fi".into()],
            ..Default::default()
        },
    );

    let resolver = MetadataResolver::new()
        .with_provider(failing)
        .with_provider(good);
    let merged = resolver.resolve(&request(MediaKind::Movie, &probe)).await;

    assert_eq!(merged.overview.as_deref(), Some("survived"));
    assert_eq!(merged.genres, vec!["Sci-Fi"]);
}

#[tokio::test]
async fn unsupported_kind_provider_is_not_consulted() {
    let probe = MediaProbe::default();
    let movies_only = MockProvider::ok(
        "movies-only",
        100,
        MetadataResult {
            overview: Some("should not appear".into()),
            ..Default::default()
        },
    );
    let resolver = MetadataResolver::new().with_provider(movies_only);

    // Resolving an Audio item: the Movie-only provider is skipped.
    let merged = resolver.resolve(&request(MediaKind::Audio, &probe)).await;
    assert_eq!(merged, MetadataResult::default());
}

#[tokio::test]
async fn empty_resolver_yields_default() {
    let probe = MediaProbe::default();
    let resolver = MetadataResolver::new();
    let merged = resolver.resolve(&request(MediaKind::Movie, &probe)).await;
    assert_eq!(merged, MetadataResult::default());
}
