//! /Items?HasSubtitles= contract.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SubtitleTrack, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_mixed_subs() -> (web::Data<AppState>, String) {
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
    // 1 + 2: have subs. 3 + 4: no subs.
    let with_subs = vec![SubtitleTrack {
        stream_index: 2,
        language: Some("eng".into()),
        codec: Some("webvtt".into()),
        title: None,
        is_default: true,
        is_forced: false,
    }];
    for (id, subs) in [(1u64, &with_subs), (2u64, &with_subs)] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: format!("With{id}"),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    subtitle_tracks: subs.clone(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    for id in [3u64, 4u64] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: format!("Without{id}"),
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

async fn names(state: web::Data<AppState>, token: &str, qs: &str) -> Vec<String> {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?{qs}&Limit=100&SortBy=SortName"))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[actix_web::test]
async fn has_subtitles_true_returns_only_subtitled() {
    let (state, token) = seed_mixed_subs().await;
    let n = names(state, &token, "HasSubtitles=true").await;
    assert_eq!(n, vec!["With1", "With2"]);
}

#[actix_web::test]
async fn has_subtitles_false_returns_only_non_subtitled() {
    let (state, token) = seed_mixed_subs().await;
    let n = names(state, &token, "HasSubtitles=false").await;
    assert_eq!(n, vec!["Without3", "Without4"]);
}

#[actix_web::test]
async fn has_subtitles_absent_returns_all() {
    let (state, token) = seed_mixed_subs().await;
    let n = names(state, &token, "").await;
    assert_eq!(n.len(), 4);
}
