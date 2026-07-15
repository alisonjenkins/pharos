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
