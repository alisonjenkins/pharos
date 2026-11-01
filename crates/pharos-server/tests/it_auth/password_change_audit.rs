//! V8/V9 invariant on `POST /Users/{id}/Password`:
//!
//!   1. Self-change MUST verify `CurrentPw` against the existing
//!      hash. A stolen session token is not enough to lock out the
//!      legitimate owner.
//!   2. Admin changing someone else's password skips the current-
//!      password check (matches Jellyfin admin behaviour).
//!   3. Non-admin trying to change another user's password is 403.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
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
    alice_id: UserId,
    bob_id: UserId,
    admin_id: UserId,
    alice_token: String,
    admin_token: String,
}

async fn seed() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let alice_pw = SecretString::new("alice-pass");
    let bob_pw = SecretString::new("bob-pass");
    let admin_pw = SecretString::new("admin-pass");
    let alice_id = UserId::new();
    let bob_id = UserId::new();
    let admin_id = UserId::new();
    stores
        .create(UserRecord {
            id: alice_id,
            name: "alice".into(),
            password_hash: auth.hash_password(&alice_pw).unwrap(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .create(UserRecord {
            id: bob_id,
            name: "bob".into(),
            password_hash: auth.hash_password(&bob_pw).unwrap(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .create(UserRecord {
            id: admin_id,
            name: "admin".into(),
            password_hash: auth.hash_password(&admin_pw).unwrap(),
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let alice_token = stores
        .issue(alice_id, "ad")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let admin_token = stores
        .issue(admin_id, "ad")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    Fixture {
        state,
        alice_id,
        bob_id,
        admin_id,
        alice_token,
        admin_token,
    }
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
async fn self_change_with_wrong_current_pw_is_rejected() {
    let f = seed().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let alice_path = f.alice_id.0.simple().to_string();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{alice_path}/Password"))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"new-pass","CurrentPw":"NOT-ALICES-PASSWORD"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 401);
    // Verify alice's password is unchanged — original still authenticates.
    let auth = BuiltinAuth::new(f.state.stores.clone());
    AuthBackend::authenticate(&auth, "alice", &SecretString::new("alice-pass"))
        .await
        .expect("alice's original password must still work");
}

#[actix_web::test]
async fn self_change_with_correct_current_pw_succeeds() {
    let f = seed().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let alice_path = f.alice_id.0.simple().to_string();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{alice_path}/Password"))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"new-alice","CurrentPw":"alice-pass"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    // New password works, old does not.
    let auth = BuiltinAuth::new(f.state.stores.clone());
    AuthBackend::authenticate(&auth, "alice", &SecretString::new("new-alice"))
        .await
        .expect("new password must authenticate");
    assert!(
        AuthBackend::authenticate(&auth, "alice", &SecretString::new("alice-pass"))
            .await
            .is_err(),
        "old password must no longer authenticate"
    );
}

#[actix_web::test]
async fn admin_can_change_other_users_password_without_current_pw() {
    let f = seed().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let bob_path = f.bob_id.0.simple().to_string();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{bob_path}/Password"))
            .insert_header(("X-Emby-Token", f.admin_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"new-bob","CurrentPw":""}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let auth = BuiltinAuth::new(f.state.stores.clone());
    AuthBackend::authenticate(&auth, "bob", &SecretString::new("new-bob"))
        .await
        .expect("admin-set password must work");
}

#[actix_web::test]
async fn non_admin_cannot_change_other_users_password() {
    let f = seed().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let bob_path = f.bob_id.0.simple().to_string();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{bob_path}/Password"))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"hack","CurrentPw":"bob-pass"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403);
    let _ = f.admin_id;
}

async fn resolves(f: &Fixture, token: &str) -> bool {
    f.state.stores.resolve(token).await.is_ok()
}

/// B235: changing a password revokes the user's other sessions, so a thief
/// holding a stolen token loses it; the caller keeps the token they used.
#[actix_web::test]
async fn self_change_keeps_callers_token_and_revokes_the_users_others() {
    let f = seed().await;
    let thief = f.state.stores.issue(f.alice_id, "thief").await.unwrap();
    let thief = thief.0.expose().to_string();
    let key = f
        .state
        .stores
        .issue(f.alice_id, "apikey:cli")
        .await
        .unwrap();
    let mut rx = f.state.revocations.subscribe();
    let app = test::init_service(build_app(f.state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Password", f.alice_id.0.simple()))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"new-alice","CurrentPw":"alice-pass"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    assert!(resolves(&f, &f.alice_token).await, "caller keeps its token");
    assert!(!resolves(&f, &thief).await, "other token must be revoked");
    assert!(resolves(&f, key.0.expose()).await, "API key must survive");
    assert_eq!(rx.try_recv().unwrap(), f.alice_id, "user is announced");
}

/// An admin reset runs on the admin's token, so all of the target's go.
#[actix_web::test]
async fn admin_reset_revokes_all_of_the_targets_tokens_and_not_the_admins() {
    let f = seed().await;
    let bob_a = f.state.stores.issue(f.bob_id, "a").await.unwrap();
    let bob_b = f.state.stores.issue(f.bob_id, "b").await.unwrap();
    let bob_key = f.state.stores.issue(f.bob_id, "apikey:cli").await.unwrap();
    let mut rx = f.state.revocations.subscribe();
    let app = test::init_service(build_app(f.state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Password", f.bob_id.0.simple()))
            .insert_header(("X-Emby-Token", f.admin_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"new-bob","CurrentPw":""}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    assert!(!resolves(&f, bob_a.0.expose()).await);
    assert!(!resolves(&f, bob_b.0.expose()).await);
    assert!(
        resolves(&f, bob_key.0.expose()).await,
        "API key must survive"
    );
    assert!(resolves(&f, &f.admin_token).await, "admin keeps its token");
    assert_eq!(rx.try_recv().unwrap(), f.bob_id);
}

/// A refused change (wrong current password) revokes nothing.
#[actix_web::test]
async fn refused_change_revokes_nothing_and_announces_nothing() {
    let f = seed().await;
    let other = f.state.stores.issue(f.alice_id, "other").await.unwrap();
    let mut rx = f.state.revocations.subscribe();
    let app = test::init_service(build_app(f.state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Password", f.alice_id.0.simple()))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"NewPw":"x","CurrentPw":"wrong"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 401);
    assert!(resolves(&f, other.0.expose()).await);
    assert!(rx.try_recv().is_err());
}

/// A login cannot name its device like an API key (`apikey:…`): such a
/// session would be listed and revoked as a key, and would survive the
/// password change that B235 relies on to evict a thief.
#[actix_web::test]
async fn login_refuses_a_device_id_shaped_like_an_api_key() {
    let f = seed().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    for device in ["apikey:x", "ApiKey:x"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/Users/AuthenticateByName")
                .insert_header((
                    "X-Emby-Authorization",
                    format!(
                        r#"MediaBrowser Client="c", Device="d", DeviceId="{device}", Version="1""#
                    ),
                ))
                .insert_header(("content-type", "application/json"))
                .set_payload(r#"{"Username":"alice","Pw":"alice-pass"}"#)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 400, "{device}");
    }
}
