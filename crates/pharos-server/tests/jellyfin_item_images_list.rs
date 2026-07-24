#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /Items/{id}/Images` — the ImageInfo LIST jellyfin-web's "Edit Images"
//! dialog fetches first (`getItemImageInfos`). It was MISSING: pharos only had
//! `/Items/{id}/Images/{type}` (the binary route), so the bare list 404'd. The
//! dialog's ajax has no `.catch`, so a 404 left it spinning forever on every
//! video. This proves the route now answers 200 with the item's advertised
//! roles.

use actix_web::{test, web, App};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_server::{api::jellyfin, middleware::LowercasePath, state::AppState, state::Stores};
use serde_json::Value;

async fn seed_movie(cache_dir: &std::path::Path) -> (web::Data<AppState>, u64) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let id = 5258522902891629388u64; // a real-shaped movie id
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
    // A local Logo sidecar → the list must include the upload-only Logo role.
    stores
        .set_artwork(id, "Logo", "local", "/media/Deadpool (2016)/logo.png")
        .await
        .unwrap();
    let state = AppState::new(stores, "t".into())
        .with_image_cache(pharos_cache::ImageCache::new(cache_dir));
    (web::Data::new(state), id)
}

#[actix_web::test]
async fn edit_images_list_is_served_not_404() {
    let cache = tempfile::tempdir().unwrap();
    let (state, id) = seed_movie(cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // jellyfin-web calls this with the PascalCase path; LowercasePath canonicalises.
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{:032x}/Images", id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "bare /Images list must not 404 (was the spinner hang); got {}",
        resp.status()
    );

    let body = test::read_body(resp).await;
    let infos: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let types: Vec<&str> = infos
        .iter()
        .filter_map(|i| i["ImageType"].as_str())
        .collect();
    // A movie advertises frame-extract Primary/Backdrop/Thumb + the Logo sidecar.
    for expect in ["Primary", "Backdrop", "Thumb", "Logo"] {
        assert!(
            types.contains(&expect),
            "list must include {expect}: {types:?}"
        );
    }
    // Backdrop is the only indexed (list) role; Primary carries no index.
    let backdrop = infos.iter().find(|i| i["ImageType"] == "Backdrop").unwrap();
    assert_eq!(backdrop["ImageIndex"], Value::from(0));
    let primary = infos.iter().find(|i| i["ImageType"] == "Primary").unwrap();
    assert!(
        primary.get("ImageIndex").is_none(),
        "single role omits index"
    );
    assert!(
        primary["ImageTag"].as_str().is_some_and(|t| !t.is_empty()),
        "every entry carries a tag so the editor thumbnail resolves"
    );
}

#[actix_web::test]
async fn unknown_item_yields_empty_list_not_error() {
    let cache = tempfile::tempdir().unwrap();
    let (state, _) = seed_movie(cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // A well-formed but absent id → empty grid, never a hang or a 500.
    let req = test::TestRequest::get()
        .uri("/Items/00000000000000000000000000000001/Images")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body = test::read_body(resp).await;
    let infos: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(infos.is_empty(), "absent item lists no images");
}
