//! `POST /Sessions/Logout` — jellyfin-web calls it on sign-out; it must end
//! the calling session (and only that one).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed() -> (web::Data<AppState>, String, String) {
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
    let a = stores.issue(uid, "device-a").await.unwrap();
    let b = stores.issue(uid, "device-b").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, a.0.expose().to_string(), b.0.expose().to_string())
}

async fn call(
    state: &web::Data<AppState>,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> StatusCode {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let mut req = if method == "POST" {
        test::TestRequest::post()
    } else {
        test::TestRequest::get()
    }
    .uri(uri);
    if let Some(t) = token {
        req = req.insert_header(("X-Emby-Token", t));
    }
    test::call_service(&app, req.to_request()).await.status()
}

#[actix_web::test]
async fn logout_revokes_the_calling_token_only() {
    let (state, a, b) = seed().await;
    assert_eq!(
        call(&state, "GET", "/Users/Me", Some(&a)).await,
        StatusCode::OK
    );

    assert_eq!(
        call(&state, "POST", "/Sessions/Logout", Some(&a)).await,
        StatusCode::NO_CONTENT
    );

    assert_eq!(
        call(&state, "GET", "/Users/Me", Some(&a)).await,
        StatusCode::UNAUTHORIZED,
        "the logged-out token is dead"
    );
    assert_eq!(
        call(&state, "GET", "/Users/Me", Some(&b)).await,
        StatusCode::OK,
        "the user's other device stays signed in"
    );
}

#[actix_web::test]
async fn logout_without_a_valid_token_is_unauthorized() {
    let (state, _a, _b) = seed().await;
    assert_eq!(
        call(&state, "POST", "/Sessions/Logout", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&state, "POST", "/Sessions/Logout", Some("bogus")).await,
        StatusCode::UNAUTHORIZED
    );
}
