//! LIB-D1 — merge-behaviour unit tests for [`MetadataResolver`].
#![allow(clippy::unwrap_used, clippy::expect_used)]

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

/// B169 — the merge is first-Some-wins across four providers, and until now
/// nothing recorded WHICH one won. That is why 139 films carrying their
/// container's mux date as a release date were indistinguishable from 139
/// films that were simply new: the value was visible, its provenance was not.
/// One counter answers "where do my years come from" for the whole library.
#[tokio::test]
async fn the_provider_that_supplied_the_year_is_recorded() {
    use metrics_util::debugging::DebuggingRecorder;

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let probe = MediaProbe::default();
    // The shape of the bug: a low-priority source holds the RIGHT year, and a
    // higher-priority one overrides it. Which is exactly what a filename
    // `(2003)` losing to a container tag looks like.
    let filenamey = MockProvider::ok(
        "filename",
        10,
        MetadataResult {
            production_year: Some(2003),
            ..Default::default()
        },
    );
    let embeddedy = MockProvider::ok(
        "embedded",
        30,
        MetadataResult {
            production_year: Some(2026),
            ..Default::default()
        },
    );
    let resolver = MetadataResolver::new()
        .with_provider(filenamey)
        .with_provider(embeddedy);
    let merged = resolver.resolve(&request(MediaKind::Movie, &probe)).await;
    assert_eq!(
        merged.production_year,
        Some(2026),
        "precondition: higher priority wins"
    );

    let found = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(ck, _, _, _)| {
            let k = ck.key();
            if k.name() != "pharos_metadata_field_source_total" {
                return None;
            }
            let labels: Vec<String> = k
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            labels
                .contains(&"field=production_year".to_string())
                .then_some(labels)
        });

    let Some(labels) = found else {
        panic!(
            "the provider that supplied the year must be recorded — without it \
             a wrong year and a right one look identical"
        );
    };
    assert!(
        labels.contains(&"provider=embedded".to_string()),
        "must name the provider that actually won, not one that merely ran; got {labels:?}"
    );
}

/// 004-books (T065) — the book provider is an ordinary participant in the
/// priority-ordered merge, not a special case bolted onto the side.
///
/// Three things this pins, each of which fails silently otherwise:
///
/// * A curated `.nfo` still wins. If the book provider outranked it, editing a
///   book's title in Kodi would appear to work and be overwritten on the next
///   scan.
/// * A title-less book falls through to the FILENAME provider, so no book is
///   ever listed untitled (FR-007). This is the reason `supports` was widened
///   to admit books — a `matches!` the compiler never flagged (R10).
/// * `dc:date` and `dc:description` land on the ITEM (production_year /
///   overview), not on `BookMeta` (R6). Two homes for one fact is two answers
///   that can disagree.
#[tokio::test]
async fn book_metadata_flows_through_the_existing_resolver() {
    use crate::metadata::book::BookMetadataProvider;
    use crate::metadata::filename::FilenameProvider;
    use metrics_util::debugging::DebuggingRecorder;
    use std::io::Write;

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let td = tempfile::tempdir().unwrap();
    let titled = td.path().join("Dune.epub");
    let untitled = td.path().join("Some Untitled Book.epub");
    let write = |path: &Path, opf: &str| {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(
            br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
        )
        .unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.finish().unwrap();
    };
    write(
        &titled,
        r#"<package><metadata>
             <dc:title>Dune</dc:title>
             <dc:description>A desert planet.</dc:description>
             <dc:date>1965-08-01</dc:date>
           </metadata><manifest/></package>"#,
    );
    write(
        &untitled,
        r#"<package><metadata><dc:creator>Anon</dc:creator></metadata><manifest/></package>"#,
    );

    let probe = MediaProbe::default();
    // An NFO-shaped high-priority source, standing in for a curated edit.
    let nfoish = MockProvider {
        name: "nfo",
        priority: 100,
        supports: MediaKind::Book,
        result: MetadataResult {
            title: Some("Dune (curated)".into()),
            ..Default::default()
        },
        fail: false,
    };
    let resolver = MetadataResolver::new()
        .with_provider(nfoish)
        .with_provider(BookMetadataProvider::new())
        .with_provider(FilenameProvider::new());

    let merged = resolver
        .resolve(&MetadataRequest {
            path: &titled,
            kind: MediaKind::Book,
            probe: &probe,
            series: None,
        })
        .await;
    assert_eq!(
        merged.title.as_deref(),
        Some("Dune (curated)"),
        "a curated source must still outrank the book file, or a user's edit is \
         silently reverted on the next scan"
    );
    assert_eq!(
        merged.overview.as_deref(),
        Some("A desert planet."),
        "dc:description belongs on the ITEM's overview, not on BookMeta (R6)"
    );
    assert_eq!(
        merged.production_year,
        Some(1965),
        "dc:date belongs on the ITEM's year, not on BookMeta (R6)"
    );

    // FR-007 — the filename supplies what the file did not. Resolved WITHOUT
    // the curated mock, which answers for every path and would otherwise mask
    // the fallback this is testing.
    let plain = MetadataResolver::new()
        .with_provider(BookMetadataProvider::new())
        .with_provider(FilenameProvider::new());
    let merged = plain
        .resolve(&MetadataRequest {
            path: &untitled,
            kind: MediaKind::Book,
            probe: &probe,
            series: None,
        })
        .await;
    assert_eq!(
        merged.title.as_deref(),
        Some("Some Untitled Book"),
        "a book whose OPF names no title takes one from its filename; without \
         the widened `supports` gate it would list untitled"
    );

    // B169's counter, reused — no new metric. It must name the BOOK provider as
    // the source of the year, not merely record that a year arrived.
    let labels = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(ck, _, _, _)| {
            let k = ck.key();
            if k.name() != "pharos_metadata_field_source_total" {
                return None;
            }
            let labels: Vec<String> = k
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            labels
                .contains(&"field=production_year".to_string())
                .then_some(labels)
        });
    let Some(labels) = labels else {
        panic!("the provider that supplied a book's year must be recorded")
    };
    assert!(
        labels.contains(&"provider=book".to_string()),
        "must name the provider that actually won; got {labels:?}"
    );
}
