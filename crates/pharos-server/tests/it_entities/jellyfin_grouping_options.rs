//! `GET /UserViews/GroupingOptions` — jellyfin-web's home-screen settings
//! page lists the libraries a user may fold into the "My Media" grouping.
//! Wire shape is `SpecialViewOptionDto[]`: `{Name, Id}`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed() -> (web::Data<AppState>, String, UserId) {
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
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into()).with_media_roots(vec!["/media/movies".into()]),
    );
    (state, token.0.expose().to_string(), uid)
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

#[actix_web::test]
async fn grouping_options_list_each_library_as_name_and_id() {
    let (state, token, _) = seed().await;
    let (status, views) = get(&state, Some(&token), "/UserViews").await;
    assert_eq!(status, StatusCode::OK);
    let expected: Vec<(String, String)> = views["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["Name"].as_str().unwrap().to_string(),
                v["Id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(!expected.is_empty());

    let (status, body) = get(
        &state,
        Some(&token),
        "/UserViews/GroupingOptions?userId=ignored",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let got: Vec<(String, String)> = body
        .as_array()
        .expect("a bare SpecialViewOptionDto array")
        .iter()
        .map(|v| {
            assert_eq!(v.as_object().unwrap().len(), 2, "only Name and Id");
            (
                v["Name"].as_str().unwrap().to_string(),
                v["Id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(got, expected);
}

#[actix_web::test]
async fn legacy_per_user_path_returns_the_same_options() {
    let (state, token, uid) = seed().await;
    let (_, a) = get(&state, Some(&token), "/UserViews/GroupingOptions").await;
    let (status, b) = get(
        &state,
        Some(&token),
        &format!("/Users/{}/GroupingOptions", uid.0.simple()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(a, b);
}

#[actix_web::test]
async fn grouping_options_require_a_token() {
    let (state, _, _) = seed().await;
    let (status, _) = get(&state, None, "/UserViews/GroupingOptions").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
