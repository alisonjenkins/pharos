//! V8 invariant: cleartext passwords never appear in any Debug
//! output. Three request bodies carry passwords —
//! AuthenticateByNameRequest, CreateUserBody, SetPasswordBody — each
//! has a custom Debug impl that renders the password as `<redacted>`.
//!
//! An accidental `tracing::debug!(?body)` or `error!(?body)`
//! anywhere in the handler chain would otherwise leak the cleartext
//! to logs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// AuthenticateByNameRequest lives in the pharos-server crate's
/// public api::jellyfin::dto module. Round-trip a body with a known
/// password marker through serde, then format with `{:?}` and grep
/// for the marker.
#[test]
fn authenticate_by_name_request_debug_redacts_pw() {
    let body: pharos_server::api::jellyfin::dto::AuthenticateByNameRequest =
        serde_json::from_str(r#"{"Username":"alice","Pw":"S3CRET-DO-NOT-LEAK"}"#).unwrap();
    let dbg = format!("{body:?}");
    assert!(dbg.contains("alice"), "username should still show: {dbg}");
    assert!(
        !dbg.contains("S3CRET-DO-NOT-LEAK"),
        "password must NOT appear in Debug output: {dbg}"
    );
    assert!(dbg.contains("<redacted>"), "must render marker: {dbg}");
}

/// CreateUserBody + SetPasswordBody are private to the admin
/// module. We can't construct them directly from outside, so this
/// test drives the HTTP path: a debug-format of the actix request
/// body would happen inside the handler if it ever did. Instead,
/// hit the wire and assert no 5xx contains the password.
#[actix_web::test]
async fn admin_create_user_response_never_echoes_password() {
    use actix_web::{test, web, App};
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
    use pharos_server::{
        api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
    };
    use pharos_store_sqlx::sqlite::SqliteStore;

    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let admin_id = UserId::new();
    stores
        .create(UserRecord {
            id: admin_id,
            name: "admin".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(admin_id, "ad").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let payload = r#"{"Name":"newuser","Password":"VERY-DISTINCT-CANARY-PASS"}"#;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Users/New")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .insert_header(("content-type", "application/json"))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert!(
        !body
            .windows(b"VERY-DISTINCT-CANARY-PASS".len())
            .any(|w| w == b"VERY-DISTINCT-CANARY-PASS"),
        "password must NOT echo back in /Users/New response"
    );
}
