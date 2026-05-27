//! /Items?ParentId={pid} contract — the documented branches in
//! `restrict_to_parent`:
//!   1. None / empty / all-media-placeholder (32 zeros) → full list.
//!   2. Library root id → items under that root only.
//!   3. Unknown id → empty list (NOT the full list).
//!
//! Why audit: a regression that flipped "unknown id" from empty to
//! full would silently surface every library item under
//! `/Items?ParentId=garbage` — the wrong tile content for any
//! genre / artist / album view jellyfin-web routes through.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
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
    for i in 1..=3u64 {
        stores
            .put(MediaItem {
                id: i,
                path: format!("/m/it{i}.mkv").into(),
                title: format!("It{i}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
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
async fn parent_id_unknown_returns_empty_list() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // 32-hex synth-shaped id with no match anywhere — must NOT fall
    // through to "full list", that would leak items into a synthetic
    // collection the user didn't browse.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?ParentId=deadbeefdeadbeefdeadbeefdeadbeef&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"].as_u64(), Some(0));
    assert!(v["Items"].as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn parent_id_all_media_placeholder_returns_full_list() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?ParentId=00000000000000000000000000000000&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["TotalRecordCount"].as_u64(),
        Some(3),
        "all-media placeholder must pass every item through"
    );
}

#[actix_web::test]
async fn parent_id_empty_string_returns_full_list() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?ParentId=&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"].as_u64(), Some(3));
}

#[actix_web::test]
async fn parent_id_absent_returns_full_list() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"].as_u64(), Some(3));
}
