//! `GET /Items/{itemId}/Ancestors` — jellyfin-web's detail-page breadcrumbs.
//! Wire shape is a bare `BaseItemDto[]`, nearest parent first, library last.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SeriesInfo, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
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
    stores
        .put(MediaItem {
            id: 1,
            path: "/media/movies/A.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe::default(),
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 2,
            path: "/media/tv/Show/Season 1/e1.mkv".into(),
            title: "E1".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/media/tv/Show".into()),
                series_year: None,
            }),
            probe: MediaProbe::default(),
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec!["/media/movies".into(), "/media/tv".into()]),
    );
    (state, token.0.expose().to_string())
}

async fn get(
    state: &web::Data<AppState>,
    token: Option<&str>,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let mut req = test::TestRequest::get().uri(uri);
    if let Some(t) = token {
        req = req.insert_header(("X-Emby-Token", t));
    }
    let resp = test::call_service(&app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn id_name_type(v: &serde_json::Value) -> Vec<(String, String)> {
    v.as_array()
        .expect("a bare BaseItemDto array")
        .iter()
        .map(|i| {
            (
                i["Type"].as_str().unwrap().to_string(),
                i["Name"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[actix_web::test]
async fn episode_ancestors_are_season_then_series_then_library() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, Some(&token), "/Items/2/Ancestors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        id_name_type(&body),
        vec![
            ("Season".to_string(), "Season 1".to_string()),
            ("Series".to_string(), "Show".to_string()),
            ("CollectionFolder".to_string(), "tv".to_string()),
        ]
    );

    let (_, ep) = get(&state, Some(&token), "/Items/2").await;
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], ep["SeasonId"].as_str().unwrap());
    assert_eq!(ids[1], ep["SeriesId"].as_str().unwrap());
}

#[actix_web::test]
async fn movie_ancestor_is_its_library() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, Some(&token), "/Items/1/Ancestors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        id_name_type(&body),
        vec![("CollectionFolder".to_string(), "movies".to_string())]
    );
}

#[actix_web::test]
async fn series_ancestor_is_its_library_and_a_library_has_none() {
    let (state, token) = seed().await;
    let (_, ep) = get(&state, Some(&token), "/Items/2").await;
    let series_id = ep["SeriesId"].as_str().unwrap().to_string();
    let (status, body) = get(
        &state,
        Some(&token),
        &format!("/Items/{series_id}/Ancestors"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        id_name_type(&body),
        vec![("CollectionFolder".to_string(), "tv".to_string())]
    );

    let library_id = body[0]["Id"].as_str().unwrap().to_string();
    let (status, body) = get(
        &state,
        Some(&token),
        &format!("/Items/{library_id}/Ancestors"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[actix_web::test]
async fn unknown_item_is_404_and_missing_token_is_401() {
    let (state, token) = seed().await;
    let (status, _) = get(&state, Some(&token), "/Items/999/Ancestors").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&state, None, "/Items/1/Ancestors").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
