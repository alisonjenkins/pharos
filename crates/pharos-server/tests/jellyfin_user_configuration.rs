//! Jellyfin 10.11/12 `POST /Users/Configuration`: the bearer saves their own
//! profile preferences; an optional `userId` query may name someone else,
//! which only an admin may do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

struct Fixture {
    state: web::Data<AppState>,
    me: UserId,
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
        state: web::Data::new(AppState::new(stores, "srv".into())),
        me,
        my_token: token.0.expose().to_string(),
        other,
    }
}

#[actix_web::test]
async fn configuration_post_updates_the_bearers_own_preferences() {
    let f = seed().await;
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
            .uri("/Users/Configuration")
            .insert_header(("X-Emby-Token", f.my_token.clone()))
            .set_json(serde_json::json!({
                "AudioLanguagePreference": "jpn",
                "PlayDefaultAudioTrack": false
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let me: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/Users/Me")
            .insert_header(("X-Emby-Token", f.my_token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(
        me["Configuration"]["AudioLanguagePreference"].as_str(),
        Some("jpn")
    );
    assert_eq!(me["Id"].as_str().unwrap(), f.me.0.simple().to_string());
}

#[actix_web::test]
async fn configuration_post_cannot_target_another_user_unless_admin() {
    let f = seed().await;
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
            .uri(&format!(
                "/Users/Configuration?userId={}",
                f.other.0.simple()
            ))
            .insert_header(("X-Emby-Token", f.my_token.clone()))
            .set_json(serde_json::json!({"AudioLanguagePreference": "jpn"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
