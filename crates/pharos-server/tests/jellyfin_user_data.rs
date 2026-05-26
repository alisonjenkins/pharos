#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /Users/{userId}/PlayedItems + /FavoriteItems + Resume (T33).

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth
        .hash_password(&SecretString::new("hunter2"))
        .unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    for (id, kind, title) in [
        (300_u64, MediaKind::Movie, "Movie A"),
        (301, MediaKind::Movie, "Movie B"),
        (302, MediaKind::Audio, "Track C"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.x").into(),
                title: title.into(),
                kind,
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "test".into()));
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
async fn mark_played_sets_played_and_increments_count() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], true);
    assert_eq!(v["PlayCount"], 1);
    assert_eq!(v["Key"], "300");
}

#[actix_web::test]
async fn mark_played_is_idempotent_after_unmark() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // Mark twice -> play_count = 2.
    for _ in 0..2 {
        let req = test::TestRequest::post()
            .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }
    // Then unmark -> played=false but play_count stays at 2.
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], false);
    assert_eq!(v["PlayCount"], 2);
}

#[actix_web::test]
async fn played_item_endpoint_rejects_user_mismatch() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/PlayedItems/300")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn played_item_404_for_unknown_item() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/9999", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn favorite_toggle_round_trips_via_get_item() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // POST favorite.
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify via GET /Items/{id} that UserData.IsFavorite is true.
    let req = test::TestRequest::get()
        .uri("/Items/301")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["UserData"]["IsFavorite"], true);

    // DELETE clears it.
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsFavorite"], false);
}

#[actix_web::test]
async fn resume_endpoint_lists_items_with_progress() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // Drive progress on item 300 so it lands in Resume.
    let req = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "300",
            "PlaySessionId": "sess-1",
            "PositionTicks": 50_000_000u64,
            "IsPaused": false
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/Resume", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["Id"], "300");
    assert_eq!(items[0]["UserData"]["PlaybackPositionTicks"], 50_000_000u64);
}

#[actix_web::test]
async fn played_items_endpoint_excludes_item_from_resume() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // First set a position, then mark played.
    let progress = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "300",
            "PlaySessionId": "sess",
            "PositionTicks": 1u64,
        }))
        .to_request();
    let _ = test::call_service(&app, progress).await;
    let mark = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let _ = test::call_service(&app, mark).await;
    // Resume should now be empty.
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/Resume", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);
}

#[actix_web::test]
async fn requires_auth_on_played_items() {
    let (state, _token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}
