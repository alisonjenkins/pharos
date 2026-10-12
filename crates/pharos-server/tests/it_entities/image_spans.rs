#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /items/{id}/images/{type}` must account for its own time in the trace.
//!
//! A production request took 5 s with 4 ms of child spans: the lookup, the
//! rescale, the encode and the body read were all invisible, so the gap could
//! not be attributed. This drives a real request through the real handler and
//! asserts each step opens a span.
//!
//! Its own test binary on purpose: span capture installs the process-wide
//! tracing subscriber, which must not race `obs::init` in the lib tests.

use actix_web::{test, web, App};
use pharos_cache::image_cache::ImageCache;
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_server::{
    api::jellyfin,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xFF, 0xFF, 0x3F,
    0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[actix_web::test]
async fn a_served_image_opens_a_span_per_step() {
    let (spans, _guard) = pharos_test_metrics::spans::capture();
    let media_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let poster = media_dir.path().join("poster.png");
    std::fs::write(&poster, PNG_1X1).unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "t".into())
            .with_image_cache(ImageCache::new(cache_dir.path().to_path_buf())),
    );
    state
        .stores
        .put(MediaItem {
            id: 55,
            path: media_dir.path().join("movie.mkv"),
            title: "A".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .stores
        .set_artwork(55, "Primary", "local", &poster.to_string_lossy())
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/items/55/images/primary")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    for step in [
        "image.item_lookup",
        "image.artwork_lookup",
        "image.resize",
        "image.etag",
        "image.read",
    ] {
        assert!(
            spans.contains(step),
            "missing span {step}; got {:?}",
            spans.names()
        );
    }
}
