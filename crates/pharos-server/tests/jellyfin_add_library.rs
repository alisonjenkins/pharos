#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Dashboard library management — POST/DELETE `/Library/VirtualFolders`.
//!
//! The wizard creates a typed library over an already-scanned path; the
//! items under it are re-stamped with the new `library_id` (no rescan) and
//! the library surfaces with its CollectionType so jellyfin-web groups it.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed(admin: bool) -> (web::Data<AppState>, String, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    // Two movies under /media/movies, one episode under /media/tv.
    for (id, path, kind) in [
        (1u64, "/media/movies/Warcraft.mkv", MediaKind::Movie),
        (2, "/media/movies/Dune.mkv", MediaKind::Movie),
        (3, "/media/tv/Show/S01E01.mkv", MediaKind::Episode),
    ] {
        stores
            .put(MediaItem {
                id,
                path: path.into(),
                title: format!("item-{id}"),
                kind,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // media_roots covers /media so add-library over a subpath does NOT spawn a
    // background scan (items are already indexed → backfill is enough).
    let state =
        web::Data::new(AppState::new(stores, "srv".into()).with_media_roots(vec!["/media".into()]));
    (state, token.0.expose().to_string(), uid)
}

fn build_app(
    state: web::Data<AppState>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

#[actix_web::test]
async fn add_virtual_folder_creates_typed_library_and_groups_items() {
    let (state, token, uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;

    // Add a Movies library over the already-scanned /media/movies path.
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"LibraryOptions":{"PathInfos":[{"Path":"/media/movies"}]}}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "add failed: {:?}",
        resp.status()
    );

    // It now shows in VirtualFolders with the movies CollectionType.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Library/VirtualFolders")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    let movies = arr
        .iter()
        .find(|f| f["Name"] == "Movies")
        .expect("Movies library missing");
    assert_eq!(movies["CollectionType"], "movies");
    let wire_id = movies["ItemId"].as_str().unwrap().to_string();

    // Browsing that library returns exactly the 2 movies under its path.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/Users/{}/Items?ParentId={wire_id}",
                uid.0.simple()
            ))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2, "expected 2 movies, got {v}");
}

#[actix_web::test]
async fn add_virtual_folder_requires_admin() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders?name=Movies&collectionType=movies")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"LibraryOptions":{"PathInfos":[{"Path":"/media/movies"}]}}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn add_virtual_folder_without_a_path_is_bad_request() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders?name=Empty&collectionType=movies")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"LibraryOptions":{"PathInfos":[]}}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn remove_virtual_folder_drops_the_library() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    // Add then remove.
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Library/VirtualFolders?name=Movies&collectionType=movies")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"LibraryOptions":{"PathInfos":[{"Path":"/media/movies"}]}}"#)
            .to_request(),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/Library/VirtualFolders?name=Movies")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "delete failed: {:?}",
        resp.status()
    );

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Library/VirtualFolders")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let has_movies = v.as_array().unwrap().iter().any(|f| f["Name"] == "Movies");
    assert!(!has_movies, "Movies library should be gone: {v}");
}
