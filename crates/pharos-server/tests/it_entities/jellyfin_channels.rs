//! `/Channels*` — the SDK ChannelsApi surface jellyfin-web 12.2 calls. Pharos
//! has no plugin channels, so lists are empty and a per-channel feature lookup
//! is a 404 (Jellyfin's `ResourceNotFoundException` for an unknown channel).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
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
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
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

fn assert_empty_page(body: &serde_json::Value) {
    assert_eq!(
        body,
        &serde_json::json!({"Items": [], "TotalRecordCount": 0, "StartIndex": 0})
    );
}

#[actix_web::test]
async fn channels_list_is_empty_page() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, Some(&token), "/Channels?startIndex=0&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_empty_page(&body);
}

#[actix_web::test]
async fn all_channel_features_is_empty_array() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, Some(&token), "/Channels/Features").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[actix_web::test]
async fn latest_channel_items_is_empty_page() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, Some(&token), "/Channels/Items/Latest?limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_empty_page(&body);
}

#[actix_web::test]
async fn channel_items_is_empty_page() {
    let (state, token) = seed().await;
    let uri = "/Channels/0a1b2c3d4e5f60718293a4b5c6d7e8f9/Items?limit=10";
    let (status, body) = get(&state, Some(&token), uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_empty_page(&body);
}

#[actix_web::test]
async fn unknown_channel_features_is_404() {
    let (state, token) = seed().await;
    let uri = "/Channels/0a1b2c3d4e5f60718293a4b5c6d7e8f9/Features";
    let (status, _) = get(&state, Some(&token), uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn channel_routes_require_auth() {
    let (state, _) = seed().await;
    for uri in [
        "/Channels",
        "/Channels/Features",
        "/Channels/Items/Latest",
        "/Channels/0a1b2c3d4e5f60718293a4b5c6d7e8f9/Items",
        "/Channels/0a1b2c3d4e5f60718293a4b5c6d7e8f9/Features",
    ] {
        let (status, _) = get(&state, None, uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}
