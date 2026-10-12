//! T019: TV-bound access tokens, exercised through the real auth extractor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use actix_web::{http::StatusCode, test, web};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    auth::BuiltinAuth,
    cast::{mint_tv_token, revoke_tv_token, CastTokenError, DEFAULT_TV_TOKEN_LIFETIME},
    state::{AppState, Stores},
};
use std::time::Duration;

async fn seed() -> (web::Data<AppState>, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "owner".into(),
            password_hash: auth.hash_password(&SecretString::new("p")).unwrap(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    (web::Data::new(AppState::new(stores, "srv".into())), uid)
}

/// The header the stock Chromecast receiver sends: its own DeviceId (base64 of
/// the receiver name), never ours.
fn receiver_auth(token: &str) -> (&'static str, String) {
    (
        "Authorization",
        format!(
            "MediaBrowser Client=\"Chromecast\", Device=\"Chromecast\", \
             DeviceId=\"TGl2aW5nIFJvb20gVFY=\", Version=\"3.0\", Token=\"{token}\""
        ),
    )
}

async fn status_of(state: &web::Data<AppState>, header: (&'static str, String)) -> StatusCode {
    let app = test::init_service(common::build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(header)
        .to_request();
    test::call_service(&app, req).await.status()
}

#[actix_web::test]
async fn tv_token_authenticates_as_owner_with_a_different_device_id() {
    let (state, uid) = seed().await;
    let tv = mint_tv_token(
        &state.stores,
        uid,
        "Living Room TV",
        DEFAULT_TV_TOKEN_LIFETIME,
    )
    .await
    .unwrap();

    let app = test::init_service(common::build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(receiver_auth(tv.secret().expose()))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["Name"], "owner");
}

#[actix_web::test]
async fn tv_token_is_bound_to_the_cast_device_name_and_hashed_at_rest() {
    let (state, uid) = seed().await;
    let tv = mint_tv_token(&state.stores, uid, "Den", DEFAULT_TV_TOKEN_LIFETIME)
        .await
        .unwrap();
    let records = state.stores.tokens_for(uid).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].device_id, "cast:Den");
    assert_ne!(tv.id.0, tv.secret().expose());
    assert_eq!(tv.id.0.len(), 64, "id is the SHA-256 hex, not the secret");
}

#[actix_web::test]
async fn revoked_tv_token_fails_auth_and_revocation_is_idempotent() {
    let (state, uid) = seed().await;
    let tv = mint_tv_token(&state.stores, uid, "Den", DEFAULT_TV_TOKEN_LIFETIME)
        .await
        .unwrap();
    let header = receiver_auth(tv.secret().expose());
    assert_eq!(status_of(&state, header.clone()).await, StatusCode::OK);

    revoke_tv_token(&state.stores, &tv.id).await.unwrap();
    revoke_tv_token(&state.stores, &tv.id).await.unwrap();
    assert_eq!(status_of(&state, header).await, StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn lifetime_is_bounded_by_the_caller() {
    let (state, uid) = seed().await;
    let tv = mint_tv_token(&state.stores, uid, "Den", Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        status_of(&state, receiver_auth(tv.secret().expose())).await,
        StatusCode::OK
    );
    assert!(matches!(
        mint_tv_token(&state.stores, uid, "Den", Duration::ZERO).await,
        Err(CastTokenError::InvalidLifetime { .. })
    ));
}

#[actix_web::test]
async fn store_failure_is_a_dependency_failure_not_an_auth_failure() {
    let (state, _uid) = seed().await;
    // A user that does not exist violates the FK, which the store reports as a
    // backend failure; it must keep its cause and be flagged as a dependency.
    let err = mint_tv_token(
        &state.stores,
        UserId::new(),
        "Den",
        DEFAULT_TV_TOKEN_LIFETIME,
    )
    .await
    .unwrap_err();
    assert!(err.is_dependency_failure(), "{err}");
    let msg = err.to_string();
    assert!(msg.contains("Den"), "names the TV: {msg}");
    assert!(std::error::Error::source(&err).is_some(), "keeps its cause");
}

#[actix_web::test]
async fn empty_tv_name_is_rejected() {
    let (state, uid) = seed().await;
    assert!(matches!(
        mint_tv_token(&state.stores, uid, "  ", DEFAULT_TV_TOKEN_LIFETIME).await,
        Err(CastTokenError::EmptyTvName)
    ));
}
