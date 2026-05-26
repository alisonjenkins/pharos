#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
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
async fn sessions_empty_on_fresh_state() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Sessions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    assert_eq!(std::str::from_utf8(&body).unwrap(), "[]");
}

#[actix_web::test]
async fn playing_then_sessions_lists_active() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    let playing = test::TestRequest::post()
        .uri("/Sessions/Playing")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "sess-1",
            "PositionTicks": 0u64
        }))
        .to_request();
    let resp = test::call_service(&app, playing).await;
    assert_eq!(resp.status(), 204);

    let list = test::TestRequest::get()
        .uri("/Sessions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, list).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["NowPlayingItemId"], "100");
    assert_eq!(arr[0]["Id"], "sess-1");
}

#[actix_web::test]
async fn progress_updates_position_and_pause() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    let playing = test::TestRequest::post()
        .uri("/Sessions/Playing")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "s1",
            "PositionTicks": 0u64
        }))
        .to_request();
    test::call_service(&app, playing).await;

    let progress = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "s1",
            "PositionTicks": 123_456_789u64,
            "IsPaused": true
        }))
        .to_request();
    test::call_service(&app, progress).await;

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v[0]["PositionTicks"], 123_456_789u64);
    assert_eq!(v[0]["IsPaused"], true);
}

#[actix_web::test]
async fn stopped_removes_session() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "ItemId": "100", "PlaySessionId":"s1", "PositionTicks": 0u64
            }))
            .to_request(),
    )
    .await;
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({ "PlaySessionId": "s1" }))
            .to_request(),
    )
    .await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(std::str::from_utf8(&body).unwrap(), "[]");
}

#[actix_web::test]
async fn capabilities_accepts_body_and_returns_204() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Capabilities")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "PlayableMediaTypes": ["Video","Audio"],
                "SupportedCommands": ["VolumeUp","VolumeDown","PlayState"]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
}

#[actix_web::test]
async fn sessions_requires_auth() {
    let (state, _t) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/Sessions").to_request(),
    )
    .await;
    assert_eq!(resp.status(), 401);
}
