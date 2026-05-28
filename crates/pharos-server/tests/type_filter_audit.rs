//! /Items?ExcludeItemTypes + /Items?MediaTypes contracts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_mixed() -> (web::Data<AppState>, String) {
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
    let rows: &[(u64, &str, MediaKind)] = &[
        (1, "M1", MediaKind::Movie),
        (2, "E1", MediaKind::Episode),
        (3, "A1", MediaKind::Audio),
    ];
    for (id, title, kind) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: (*title).into(),
                kind: *kind,
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

async fn names_for(state: web::Data<AppState>, token: &str, qs: &str) -> Vec<String> {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?{qs}&Limit=100"))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let mut names: Vec<String> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect();
    names.sort();
    names
}

#[actix_web::test]
async fn exclude_item_types_drops_listed_kinds() {
    let (state, token) = seed_mixed().await;
    let names = names_for(state, &token, "ExcludeItemTypes=Episode").await;
    assert_eq!(names, vec!["A1", "M1"]);
}

#[actix_web::test]
async fn exclude_item_types_is_case_insensitive() {
    let (state, token) = seed_mixed().await;
    let names = names_for(state, &token, "ExcludeItemTypes=audio,episode").await;
    assert_eq!(names, vec!["M1"]);
}

#[actix_web::test]
async fn media_types_audio_picks_audio_only() {
    let (state, token) = seed_mixed().await;
    let names = names_for(state, &token, "MediaTypes=Audio").await;
    assert_eq!(names, vec!["A1"]);
}

#[actix_web::test]
async fn media_types_video_picks_movie_and_episode() {
    let (state, token) = seed_mixed().await;
    let names = names_for(state, &token, "MediaTypes=Video").await;
    assert_eq!(names, vec!["E1", "M1"]);
}

#[actix_web::test]
async fn media_types_both_returns_all() {
    let (state, token) = seed_mixed().await;
    let names = names_for(state, &token, "MediaTypes=Audio,Video").await;
    assert_eq!(names, vec!["A1", "E1", "M1"]);
}
