//! V9 invariant on `GET /Sessions`: non-admin bearers see only their
//! own sessions; admin bearers see all. A bare `_user: AuthUser`
//! handler that returns the full snapshot leaks now-playing item ids
//! across tenants.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    sessions::SessionEvent,
    state::{AppState, Stores},
};

async fn seed_two_users_and_sessions() -> (web::Data<AppState>, String, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let alice = UserId::new();
    let bob = UserId::new();
    stores
        .create(UserRecord {
            id: alice,
            name: "alice".into(),
            password_hash: hash.clone(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .create(UserRecord {
            id: bob,
            name: "bob".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let alice_t = stores
        .issue(alice, "ad")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let bob_t = stores
        .issue(bob, "bd")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    // Each user starts one playback session.
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id: "alice-sess".into(),
            user_id: alice,
            user_name: "alice".into(),
            device_id: "ad".into(),
            device_name: "Alice Phone".into(),
            client: "x".into(),
            version: "1".into(),
            item_id: "42".into(),
            position_ticks: 0,
        })
        .await
        .unwrap();
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id: "bob-sess".into(),
            user_id: bob,
            user_name: "bob".into(),
            device_id: "bd".into(),
            device_name: "Bob TV".into(),
            client: "x".into(),
            version: "1".into(),
            item_id: "99".into(),
            position_ticks: 0,
        })
        .await
        .unwrap();
    (state, alice_t, bob_t)
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
async fn non_admin_bearer_sees_only_own_session() {
    let (state, alice_t, _bob_t) = seed_two_users_and_sessions().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", alice_t.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("top-level array");
    assert_eq!(arr.len(), 1, "alice must see only her own session; got {v}");
    assert_eq!(arr[0]["UserName"].as_str(), Some("alice"));
    assert_eq!(arr[0]["NowPlayingItemId"].as_str(), Some("42"));
}

#[actix_web::test]
async fn admin_bearer_sees_every_session() {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let admin_uid = UserId::new();
    let user_uid = UserId::new();
    stores
        .create(UserRecord {
            id: admin_uid,
            name: "admin".into(),
            password_hash: hash.clone(),
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    stores
        .create(UserRecord {
            id: user_uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let admin_t = stores
        .issue(admin_uid, "ad")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    // One session per user.
    for (id, sess_id) in [(admin_uid, "a"), (user_uid, "u")] {
        state
            .sessions
            .apply(SessionEvent::Started {
                session_id: sess_id.into(),
                user_id: id,
                user_name: format!("user-{sess_id}"),
                device_id: sess_id.into(),
                device_name: "dev".into(),
                client: "x".into(),
                version: "1".into(),
                item_id: "1".into(),
                position_ticks: 0,
            })
            .await
            .unwrap();
    }
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", admin_t.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("top-level array");
    assert_eq!(arr.len(), 2, "admin sees both sessions; got {v}");
}
