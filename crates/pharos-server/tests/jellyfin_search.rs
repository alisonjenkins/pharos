#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /Search/Hints + /Search/Suggestions (T32).

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    for (id, kind, title) in [
        (200_u64, MediaKind::Movie, "Blade Runner"),
        (201, MediaKind::Movie, "Blade Runner 2049"),
        (202, MediaKind::Audio, "Vangelis - Tales of the Future"),
        (203, MediaKind::Episode, "The Expanse - S01E01"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.x").into(),
                title: title.into(),
                kind,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "test".into()));
    (state, token.0.expose().to_string(), uid)
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
async fn search_hints_requires_auth() {
    let (state, _t, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Hints?searchTerm=blade")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn search_hints_matches_title_substring_case_insensitive() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Hints?searchTerm=BLADE")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
    let hints = v["SearchHints"].as_array().unwrap();
    assert_eq!(hints.len(), 2);
    let names: Vec<&str> = hints.iter().map(|h| h["Name"].as_str().unwrap()).collect();
    assert!(names
        .iter()
        .all(|n| n.to_ascii_lowercase().contains("blade")));
    // Hint shape: ItemId + Id duplicated, Type + MediaType present.
    let first = &hints[0];
    assert!(first["ItemId"].is_string());
    assert_eq!(first["ItemId"], first["Id"]);
    assert!(first["Type"].is_string());
    assert!(first["MediaType"].is_string());
    assert_eq!(first["MatchedTerm"], "BLADE");
}

#[actix_web::test]
async fn search_hints_empty_term_returns_full_corpus_capped_by_limit() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Hints?limit=2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Total = 4 seeded items, hint page = 2.
    assert_eq!(v["TotalRecordCount"], 4);
    assert_eq!(v["SearchHints"].as_array().unwrap().len(), 2);
}

#[actix_web::test]
async fn search_hints_filters_by_include_item_types() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Hints?searchTerm=&includeItemTypes=Audio")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["SearchHints"][0]["Type"], "Audio");
    assert_eq!(v["SearchHints"][0]["MediaType"], "Audio");
}

#[actix_web::test]
async fn search_hints_pagination() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Hints?startIndex=2&limit=2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 4);
    let hints = v["SearchHints"].as_array().unwrap();
    assert_eq!(hints.len(), 2);
}

#[actix_web::test]
async fn search_suggestions_returns_envelope_shape() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Search/Suggestions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Real impl since T-fix-41 — surfaces random unwatched items.
    // Shape stays the same: Items array + TotalRecordCount + StartIndex.
    assert!(v["Items"].is_array());
    assert_eq!(v["StartIndex"], 0);
    assert_eq!(
        v["TotalRecordCount"].as_u64().unwrap() as usize,
        v["Items"].as_array().unwrap().len()
    );
}

#[actix_web::test]
async fn user_suggestions_path_matches_bearer() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Suggestions", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn user_suggestions_rejects_mismatched_user() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/Suggestions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn search_hints_lowercase_alias_also_works() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/search/hints?searchTerm=blade")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
}
