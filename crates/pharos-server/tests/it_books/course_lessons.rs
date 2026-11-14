//! B1423 / V1392 — a course lesson stored as a film, with a film's match,
//! description and poster, is a lesson after the next scan: an episode of
//! its course, marked `course`, its film text gone, its provider poster row
//! and cached file removed.

use pharos_cache::image_cache::{image_path, ImageCache, ImageRole};
use pharos_core::{MediaKind, MediaStore, ProbeInfo};
use pharos_scanner::fs::{stable_id, FsScanner};
use pharos_server::book_cover::ImageCacheCoverSink;
use pharos_server::state::Stores;
use tempfile::TempDir;

/// Reads every file as a film, as ffprobe does for a lesson's `.mp4`.
struct FilmProber;

impl pharos_core::Prober for FilmProber {
    async fn probe(&self, _path: &std::path::Path) -> pharos_core::DomainResult<ProbeInfo> {
        Ok(ProbeInfo {
            kind: MediaKind::Movie,
            probe: pharos_core::MediaProbe {
                container: Some("mp4".into()),
                duration_ms: Some(60_000),
                ..Default::default()
            },
        })
    }
}

fn counter(
    snap: &pharos_test_metrics::CumulativeSnapshotter,
    name: &str,
    label: (&str, &str),
) -> u64 {
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter_map(|(ck, _, _, v)| match v {
            metrics_util::debugging::DebugValue::Counter(n)
                if ck.key().name() == name
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == label.0 && l.value() == label.1) =>
            {
                Some(n)
            }
            _ => None,
        })
        .sum()
}

#[tokio::test]
async fn a_rescan_turns_a_matched_film_into_a_course_lesson() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let td = TempDir::new().unwrap();
    let books = td.path().join("Books");
    let lesson = books.join("Intro to Widgets/02 - Assembly/03_-_0201_Fitting_the_Base.mp4");
    std::fs::create_dir_all(lesson.parent().unwrap()).unwrap();
    std::fs::write(&lesson, b"not really a video").unwrap();
    let cache_dir = td.path().join("cache");
    let stores = Stores::connect("sqlite::memory:").await.unwrap();

    // Before the course rule: a film, matched online.
    FsScanner::new(FilmProber)
        .scan_into(&books, &stores)
        .await
        .unwrap();
    let id = stable_id(&lesson);
    let mut film = stores.get(id).await.unwrap();
    assert_eq!(film.kind, MediaKind::Movie);
    film.metadata.overview = Some("A heist across three continents.".into());
    film.metadata.production_year = Some(1999);
    stores.put(film).await.unwrap();
    stores
        .set_item_match(id, "tmdb", "4242", "search", Some(0.8), 1)
        .await
        .unwrap();
    let poster = image_path(&cache_dir, ImageRole::Primary, MediaKind::Movie, id, 0);
    std::fs::create_dir_all(poster.parent().unwrap()).unwrap();
    std::fs::write(&poster, b"jpeg").unwrap();
    stores
        .set_artwork(id, "Primary", "tmdb", &poster.to_string_lossy())
        .await
        .unwrap();

    // The file is unchanged; the scanner now knows the Books root.
    FsScanner::new(FilmProber)
        .with_course_roots([books.clone()])
        .with_cover_sink(std::sync::Arc::new(ImageCacheCoverSink::new(
            ImageCache::new(&cache_dir),
        )))
        .scan_into(&books, &stores)
        .await
        .unwrap();

    let item = stores.get(id).await.unwrap();
    assert_eq!(item.kind, MediaKind::Episode);
    assert_eq!(item.title, "Fitting the Base");
    let series = item.series.as_ref().unwrap();
    assert_eq!(series.series_name, "Intro to Widgets");
    assert_eq!(
        (series.season_number, series.episode_number),
        (Some(2), Some(201))
    );
    assert_eq!(item.match_source.as_deref(), Some("course"));
    assert_eq!(item.match_provider.as_deref(), Some("local"));
    assert_eq!(item.match_external_id.as_deref(), Some(""));
    assert_eq!(
        item.metadata.overview, None,
        "the film's description is gone"
    );
    assert_eq!(item.metadata.production_year, None);
    assert!(
        stores.artwork_for(id).await.unwrap().is_empty(),
        "the provider poster row is gone"
    );
    assert!(!poster.exists(), "the cached provider poster is removed");
    assert!(
        stores
            .items_needing_match(10, i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "a lesson is never matched again"
    );

    assert_eq!(
        counter(
            &snap,
            "pharos_scan_course_lesson_total",
            ("outcome", "classified")
        ),
        1
    );
    assert_eq!(
        counter(
            &snap,
            "pharos_scan_reprobe_total",
            ("reason", "course_lesson")
        ),
        1
    );
    assert_eq!(
        counter(
            &snap,
            "pharos_scan_match_reset_total",
            ("reason", "course_lesson")
        ),
        1
    );
}
