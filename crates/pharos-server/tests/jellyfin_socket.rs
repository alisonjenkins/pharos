#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Smoke tests for /socket. Full bidirectional WS roundtrip is tested in
//! the group actor unit tests; here we just confirm the route is wired,
//! enforces auth, and rejects non-upgrade requests.

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, state::AppState};
use pharos_sync::GroupRegistry;
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, web::Data<GroupRegistry>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
    let token = stores.issue(uid, "test").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    let registry = web::Data::new(GroupRegistry::spawn());
    (state, registry, token.0.expose().to_string())
}

#[actix_web::test]
async fn socket_requires_auth() {
    let (state, registry, _) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .app_data(registry)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::get().uri("/socket").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn socket_authed_without_upgrade_headers_is_bad_request() {
    // AuthUser passes, actix_ws::handle returns Err because the request
    // lacks `Connection: Upgrade` and `Upgrade: websocket`. Manifests as
    // 400 in the response.
    let (state, registry, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .app_data(registry)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::get()
        .uri("/socket")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        matches!(resp.status().as_u16(), 400 | 426),
        "expected 400 Bad Request or 426 Upgrade Required, got {}",
        resp.status()
    );
}
