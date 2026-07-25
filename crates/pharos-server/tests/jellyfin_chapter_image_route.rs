#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /Items/{id}/Images/chapter/{index}` — the chapter-thumbnail route.
//!
//! It was registered AFTER the generic `/{image_type}/{image_index}` route, and
//! actix matches routes in registration order, so every chapter request was
//! swallowed by the generic handler. That handler parses the path segment as an
//! `ImageRole`, `chapter` is not one, and it answered `400 unknown image type`.
//! Observed live 2026-07-25: seven of them in one burst while jellyfin-web
//! rendered the chapter strip for a playing title.
//!
//! A route shadowed by an earlier registration is invisible to any test that
//! calls its handler directly, so this drives the REGISTERED app.

use actix_web::{test, web, App};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_server::{api::jellyfin, middleware::LowercasePath, state::AppState, state::Stores};

async fn seed_movie(cache_dir: &std::path::Path) -> (web::Data<AppState>, u64) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let id = 5258522902891629388u64;
    stores
        .put(MediaItem {
            id,
            path: "/media/Deadpool (2016)/Deadpool.mp4".into(),
            title: "Deadpool".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = AppState::new(stores, "t".into())
        .with_image_cache(pharos_cache::ImageCache::new(cache_dir));
    (web::Data::new(state), id)
}

/// The seeded item carries NO chapters, so the chapter handler's own answer is
/// `404 chapter index out of range`. That is the point: 404 proves the request
/// reached the chapter handler, and 400 proves it was shadowed by the generic
/// image-type route. The distinction is the whole test.
#[actix_web::test]
async fn a_chapter_image_request_reaches_the_chapter_handler() {
    let cache = tempfile::tempdir().unwrap();
    let (state, id) = seed_movie(cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!(
            "/Items/{id:032x}/Images/chapter/0?maxWidth=400&quality=90"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_ne!(
        resp.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "`chapter` must not be parsed as an ImageRole — the generic \
         /{{image_type}}/{{image_index}} route is shadowing the chapter route"
    );
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::NOT_FOUND,
        "an item with no chapters answers 'chapter index out of range'"
    );
}

/// The fix must not cost the generic indexed route its own dispatch: a real
/// `ImageRole` with an index still resolves, and a genuinely unknown type is
/// still a 400.
#[actix_web::test]
async fn the_generic_indexed_image_route_still_dispatches() {
    let cache = tempfile::tempdir().unwrap();
    let (state, id) = seed_movie(cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/Items/{id:032x}/Images/backdrop/0"))
        .to_request();
    assert_ne!(
        test::call_service(&app, req).await.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "a real indexed role must still parse"
    );

    let req = test::TestRequest::get()
        .uri(&format!("/Items/{id:032x}/Images/notarole/0"))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "an unknown image type is still rejected"
    );
}
