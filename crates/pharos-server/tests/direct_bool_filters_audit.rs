//! /Items direct-boolean shortcuts + episode-picker filters.
//! - IsFavorite=true / IsPlayed=true — alias for Filters=...
//! - MinIndexNumber / MaxIndexNumber — clamp episode numbers for
//!   season detail view.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, SeriesInfo, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_episodes_and_favs() -> (web::Data<AppState>, String) {
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
    // Five episodes of one series, numbered 1..=5.
    for n in 1..=5u32 {
        stores
            .put(MediaItem {
                id: u64::from(n),
                path: format!("/m/s1e{n}.mkv").into(),
                title: format!("E{n}"),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "Show".into(),
                    season_number: Some(1),
                    episode_number: Some(n),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // Favorite the middle episode.
    stores
        .set_user_data(
            uid,
            3,
            UserItemData {
                is_favorite: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Mark E5 as played.
    stores
        .set_user_data(
            uid,
            5,
            UserItemData {
                played: true,
                play_count: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
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
async fn is_favorite_true_returns_only_favorites() {
    let (state, token) = seed_episodes_and_favs().await;
    let names = names_for(state, &token, "IsFavorite=true").await;
    assert_eq!(names, vec!["E3"]);
}

#[actix_web::test]
async fn is_favorite_false_returns_only_non_favorites() {
    let (state, token) = seed_episodes_and_favs().await;
    let names = names_for(state, &token, "IsFavorite=false").await;
    assert_eq!(names, vec!["E1", "E2", "E4", "E5"]);
}

#[actix_web::test]
async fn is_played_true_returns_only_played() {
    let (state, token) = seed_episodes_and_favs().await;
    let names = names_for(state, &token, "IsPlayed=true").await;
    assert_eq!(names, vec!["E5"]);
}

#[actix_web::test]
async fn min_max_index_number_clamps_episodes() {
    let (state, token) = seed_episodes_and_favs().await;
    let names = names_for(state, &token, "MinIndexNumber=2&MaxIndexNumber=4").await;
    assert_eq!(names, vec!["E2", "E3", "E4"]);
}

#[actix_web::test]
async fn min_index_number_alone_keeps_upper_episodes() {
    let (state, token) = seed_episodes_and_favs().await;
    let names = names_for(state, &token, "MinIndexNumber=4").await;
    assert_eq!(names, vec!["E4", "E5"]);
}
