//! Jellyfin 10.11/12 `POST /Users/Password`: the bearer changes their own
//! password; an optional `userId` query may name someone else, which only an
//! admin may do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    AuthBackend, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

struct Fixture {
    state: web::Data<AppState>,
    stores: Stores,
    my_token: String,
    other: UserId,
}

async fn add_user(stores: &Stores, name: &str) -> UserId {
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let id = UserId::new();
    stores
        .create(UserRecord {
            id,
            name: name.into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    id
}

async fn seed() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let me = add_user(&stores, "me").await;
    let other = add_user(&stores, "other").await;
    let token = stores.issue(me, "dev").await.unwrap();
    Fixture {
        state: web::Data::new(AppState::new(stores.clone(), "srv".into())),
        stores,
        my_token: token.0.expose().to_string(),
        other,
    }
}

async fn post_json(
    f: &Fixture,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = test::init_service(
        App::new()
            .app_data(f.state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(uri)
            .insert_header(("X-Emby-Token", f.my_token.clone()))
            .set_json(body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn can_log_in(f: &Fixture, name: &str, pw: &str) -> bool {
    let auth = BuiltinAuth::new(f.stores.clone());
    auth.authenticate(name, &SecretString::new(pw))
        .await
        .is_ok()
}

#[actix_web::test]
async fn password_change_for_self_needs_the_current_password() {
    let f = seed().await;
    let (status, _) = post_json(
        &f,
        "/Users/Password",
        serde_json::json!({"CurrentPw": "p", "NewPw": "q"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(can_log_in(&f, "me", "q").await, "new password works");
    assert!(!can_log_in(&f, "me", "p").await, "old password is gone");
}

#[actix_web::test]
async fn password_change_with_wrong_current_password_is_rejected() {
    let f = seed().await;
    let (status, _) = post_json(
        &f,
        "/Users/Password",
        serde_json::json!({"CurrentPw": "wrong", "NewPw": "q"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(can_log_in(&f, "me", "p").await, "password unchanged");
}

#[actix_web::test]
async fn non_admin_cannot_change_someone_elses_password() {
    let f = seed().await;
    let uri = format!("/Users/Password?userId={}", f.other.0.simple());
    let (status, _) = post_json(&f, &uri, serde_json::json!({"NewPw": "q"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(can_log_in(&f, "other", "p").await);
}

/// V174: the CurrentPw check is a password check, so a stolen bearer cannot
/// use it to guess the account's password at hash speed. Past the per-user
/// limit even the right password is held with 429 and `Retry-After`.
#[actix_web::test]
async fn guessing_the_current_password_is_throttled() {
    let f = seed().await;
    let limit = pharos_server::config::LoginThrottleConfig::default().max_failures_per_user;
    for _ in 0..limit {
        let (status, _) = post_json(
            &f,
            "/Users/Password",
            serde_json::json!({"CurrentPw": "guess", "NewPw": "q"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _) = post_json(
        &f,
        "/Users/Password",
        serde_json::json!({"CurrentPw": "p", "NewPw": "q"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(can_log_in(&f, "me", "p").await, "password unchanged");
}
