//! /Items letter-jump nav: NameStartsWith / NameStartsWithOrGreater
//! / NameLessThan. jellyfin-web's A-Z chip strip relies on these.

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

async fn seed_alphabet() -> (web::Data<AppState>, String) {
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
    let titles = [
        "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Über", "Pokémon", "1984",
    ];
    for (i, t) in titles.iter().enumerate() {
        stores
            .put(MediaItem {
                id: (i + 1) as u64,
                path: format!("/m/{i}.mkv").into(),
                title: (*t).into(),
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
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[actix_web::test]
async fn name_starts_with_filters_to_prefix() {
    let (state, token) = seed_alphabet().await;
    let names = names_for(state, &token, "NameStartsWith=A").await;
    assert_eq!(names, vec!["Alpha"]);
}

#[actix_web::test]
async fn name_starts_with_is_case_insensitive() {
    let (state, token) = seed_alphabet().await;
    let names = names_for(state, &token, "NameStartsWith=b").await;
    assert_eq!(names, vec!["Bravo"]);
}

#[actix_web::test]
async fn name_starts_with_unicode_prefix_matches_accented_title() {
    let (state, token) = seed_alphabet().await;
    // Lowercase ü as prefix must match "Über".
    let names = names_for(state, &token, "NameStartsWith=%C3%BC").await;
    assert!(names.contains(&"Über".to_string()), "got {names:?}");
}

#[actix_web::test]
async fn name_starts_with_or_greater_is_alias_for_starts_with() {
    let (state, token) = seed_alphabet().await;
    let names = names_for(state, &token, "NameStartsWithOrGreater=C").await;
    assert_eq!(names, vec!["Charlie"]);
}

#[actix_web::test]
async fn name_less_than_drops_items_at_or_after_bound() {
    let (state, token) = seed_alphabet().await;
    // < "C" → "Alpha", "Bravo", "1984" (digit < alpha in lowercase
    // byte order so "1984" passes too). Default SortName order.
    let names = names_for(state, &token, "NameLessThan=C&SortBy=SortName").await;
    let set: std::collections::BTreeSet<String> = names.into_iter().collect();
    assert!(set.contains("Alpha"));
    assert!(set.contains("Bravo"));
    assert!(!set.contains("Charlie"));
    assert!(!set.contains("Delta"));
}
