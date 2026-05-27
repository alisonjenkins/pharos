//! Unicode case-insensitive search audit. Titles with accents
//! (Pokémon, Café, Über) must match queries typed in any case.
//! ASCII-only `to_ascii_lowercase` silently failed because it
//! leaves É / é / Ü untouched.

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

async fn seed_unicode_titles() -> (web::Data<AppState>, String) {
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
    for (i, title) in ["Pokémon", "Café", "Über Alles", "ASCII Only"]
        .iter()
        .enumerate()
    {
        stores
            .put(MediaItem {
                id: (i + 1) as u64,
                path: format!("/m/{i}.mkv").into(),
                title: (*title).into(),
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
async fn items_search_matches_uppercase_accented_query_to_lowercase_title() {
    let (state, token) = seed_unicode_titles().await;
    let app = test::init_service(build_app(state)).await;
    // Query in uppercase with accent. Must match "Pokémon".
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?SearchTerm=POK%C3%89MON&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<String> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "Pokémon"),
        "uppercase accented query must match accented title; got {names:?}"
    );
}

#[actix_web::test]
async fn items_search_matches_lowercase_query_to_accented_title() {
    let (state, token) = seed_unicode_titles().await;
    let app = test::init_service(build_app(state)).await;
    // Query lowercase. Must match "Über Alles".
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?SearchTerm=%C3%BCber&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<String> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "Über Alles"),
        "lowercase accented query must match; got {names:?}"
    );
}

#[actix_web::test]
async fn search_hints_matches_accented_query() {
    let (state, token) = seed_unicode_titles().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Search/Hints?searchTerm=caf%C3%A9&limit=10")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<String> = v["SearchHints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["Name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "Café"),
        "Search/Hints must match accented query; got {names:?}"
    );
}
