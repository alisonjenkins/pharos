//! /Items?IncludeItemTypes case-insensitive parsing audit.
//! Some clients (Finamp's iOS app, custom integrations) send
//! lowercase type names — real Jellyfin accepts both cases. We
//! folded only PascalCase before this fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_mixed_kinds() -> (web::Data<AppState>, String) {
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
    let rows: &[(u64, &str, MediaKind)] = &[
        (1, "Movie A", MediaKind::Movie),
        (2, "Episode B", MediaKind::Episode),
        (3, "Audio C", MediaKind::Audio),
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

#[actix_web::test]
async fn include_item_types_accepts_lowercase() {
    let (state, token) = seed_mixed_kinds().await;
    let app = test::init_service(build_app(state)).await;
    // Lowercase "audio" must match the Audio item.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?IncludeItemTypes=audio&Limit=100")
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
    assert_eq!(names, vec!["Audio C"], "lowercase type name must match");
}

#[actix_web::test]
async fn include_item_types_accepts_pascalcase() {
    let (state, token) = seed_mixed_kinds().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?IncludeItemTypes=Movie&Limit=100")
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
    assert_eq!(names, vec!["Movie A"]);
}

#[actix_web::test]
async fn include_item_types_mixed_case_in_list() {
    let (state, token) = seed_mixed_kinds().await;
    let app = test::init_service(build_app(state)).await;
    // Mixed case in one comma list — each token must fold.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?IncludeItemTypes=movie,EPISODE&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: std::collections::BTreeSet<String> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        names,
        ["Movie A", "Episode B"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
}
