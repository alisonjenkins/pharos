//! /Items?Genres=... contract — restrict the list to items whose
//! `probe.genre` matches one of the named genres. Wire convention
//! splits on `|` (Jellyfin's default) AND `,` (some clients).
//! Case-insensitive.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_with_genres() -> (web::Data<AppState>, String) {
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
    let rows: &[(u64, &str, Option<&str>)] = &[
        (1, "A", Some("Action")),
        (2, "B", Some("Drama")),
        (3, "C", Some("Comedy")),
        (4, "D", None),
    ];
    for (id, title, genre) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: (*title).into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    genre: genre.map(|g| g.to_string()),
                    ..Default::default()
                },
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

async fn names_for_genres(
    state: web::Data<AppState>,
    token: &str,
    q: &str,
) -> std::collections::BTreeSet<String> {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?Genres={q}&Limit=100"))
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
async fn single_genre_filters_to_matching_items() {
    let (state, token) = seed_with_genres().await;
    let names = names_for_genres(state, &token, "Action").await;
    assert_eq!(names, ["A"].iter().map(|s| s.to_string()).collect());
}

#[actix_web::test]
async fn pipe_separated_genres_union() {
    let (state, token) = seed_with_genres().await;
    let names = names_for_genres(state, &token, "Action%7CDrama").await;
    assert_eq!(names, ["A", "B"].iter().map(|s| s.to_string()).collect());
}

#[actix_web::test]
async fn comma_separated_genres_union() {
    let (state, token) = seed_with_genres().await;
    let names = names_for_genres(state, &token, "Action,Comedy").await;
    assert_eq!(names, ["A", "C"].iter().map(|s| s.to_string()).collect());
}

#[actix_web::test]
async fn genre_match_is_case_insensitive() {
    let (state, token) = seed_with_genres().await;
    let names = names_for_genres(state, &token, "drama").await;
    assert_eq!(names, ["B"].iter().map(|s| s.to_string()).collect());
}

#[actix_web::test]
async fn items_without_genre_tag_drop_out_when_filter_active() {
    let (state, token) = seed_with_genres().await;
    // "D" has no genre — must NOT appear under any genre filter.
    let names = names_for_genres(state, &token, "Action%7CDrama%7CComedy").await;
    assert!(
        !names.contains("D"),
        "items without genre must drop; got {names:?}"
    );
    assert_eq!(names.len(), 3);
}
