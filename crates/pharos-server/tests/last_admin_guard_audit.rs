//! "Don't brick the dashboard" guards on admin admin-related ops:
//!
//! 1. Cannot delete the last admin (delete_user already enforced
//!    self-delete refusal; this test pins the broader rule).
//! 2. Cannot demote the last admin via set_user_policy.
//!
//! These guards exist so a freshly-deployed pharos with one admin
//! cannot lock its owner out by a single mis-click.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_single_admin() -> (web::Data<AppState>, UserId, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let admin = UserId::new();
    stores
        .create(UserRecord {
            id: admin,
            name: "admin".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores
        .issue(admin, "d")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, admin, token)
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
async fn last_admin_cannot_demote_self() {
    let (state, admin_id, token) = seed_single_admin().await;
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Policy", admin_id.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"IsAdministrator":false}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    // Still admin afterwards.
    let r = state.stores.get(admin_id).await.unwrap();
    assert!(r.policy.admin, "admin must remain admin");
}

#[actix_web::test]
async fn last_admin_self_delete_blocked() {
    let (state, admin_id, token) = seed_single_admin().await;
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/Users/{}", admin_id.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[actix_web::test]
async fn admin_can_demote_self_when_another_admin_exists() {
    let (state, admin_id, token) = seed_single_admin().await;
    // Create a second admin.
    let auth = BuiltinAuth::new(state.stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    state
        .stores
        .create(UserRecord {
            id: UserId::new(),
            name: "second".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();

    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Policy", admin_id.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"IsAdministrator":false}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let r = state.stores.get(admin_id).await.unwrap();
    assert!(!r.policy.admin, "self-demotion succeeds when peers remain");
}
