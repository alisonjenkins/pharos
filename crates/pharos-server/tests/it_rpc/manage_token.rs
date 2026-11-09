//! B920 / V880: `Session.manageToken` gives an admin's web page a short-lived
//! token for the admin endpoints of the Jellyfin API, which no cookie may
//! authorize (V191). Only an admin gets one, it ends with the session that
//! asked for it, and it lives an hour at most.
//! Disarm check: dropping the `policy.admin` test in `manage_token` fails
//! `a_user_who_is_not_an_admin_gets_no_manage_token`; issuing it with
//! `issue_with_ttl` (no parent) fails `signing_out_ends_the_manage_token`;
//! listing child tokens in `devices_list` fails
//! `the_device_list_shows_the_session_not_its_manage_tokens`.

use std::rc::Rc;

use actix_web::{http::StatusCode, test, web};
use metrics_util::debugging::DebuggingRecorder;
use pharos_api::pharos_capnp::{bootstrap, error, media_token_result, session};
use pharos_api::Connection;
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::api::pharos::rpc;
use pharos_server::auth::BuiltinAuth;
use pharos_server::state::{AppState, Stores};

use crate::bootstrap::LOCAL;
use crate::common::build_app;
use crate::login::login;

/// An admin ("boss") and a user who is not ("guest"), both "hunter2".
async fn state() -> web::Data<AppState> {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let hash = BuiltinAuth::new(stores.clone())
        .hash_password(&SecretString::new("hunter2"))
        .unwrap();
    for (name, admin) in [("boss", true), ("guest", false)] {
        stores
            .create(UserRecord {
                id: UserId::new(),
                name: name.into(),
                password_hash: hash.clone(),
                policy: UserPolicy {
                    admin,
                    ..UserPolicy::default()
                },
            })
            .await
            .unwrap();
    }
    web::Data::new(AppState::new(stores, "pharos-manage".into()))
}

async fn signed_in(state: &web::Data<AppState>, name: &str) -> session::Client {
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    let conn = Rc::new(Connection::new());
    let client: bootstrap::Client = rpc::bootstrap_client(state.clone(), ctx, &conn);
    let (session, _) = login(&client, name, "hunter2").await.ok().unwrap();
    session
}

/// The token and its expiry, or the error's kind name.
async fn manage_token(session: &session::Client) -> Result<(String, u64), &'static str> {
    let mut req = session.manage_token_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        media_token_result::Token(t) => {
            let t = t.unwrap();
            Ok((
                t.get_token().unwrap().to_string().unwrap(),
                t.get_expires_at_ms(),
            ))
        }
        media_token_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::Revoked(()) => "revoked",
            error::kind::NotPermitted(_) => "not_permitted",
            error::kind::Unavailable(_) => "unavailable",
            _ => "other",
        }),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// What `GET /Users` with the token in the header answers: an admin-only
/// listing, so it shows the token carries the admin's rights.
async fn list_users_status(state: &web::Data<AppState>, token: &str) -> StatusCode {
    let app = test::init_service(build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", token))
        .to_request();
    match test::try_call_service(&app, req).await {
        Ok(resp) => resp.status(),
        Err(e) => e.as_response_error().status_code(),
    }
}

#[actix_web::test]
async fn an_admin_gets_a_manage_token_that_reaches_the_admin_endpoints() {
    let state = state().await;
    let session = signed_in(&state, "boss").await;
    let (token, expires_at_ms) = manage_token(&session).await.unwrap();
    let now = now_ms();
    assert!(
        expires_at_ms > now + 3_500_000 && expires_at_ms <= now + 3_700_000,
        "a manage token lives an hour: {expires_at_ms} vs {now}"
    );
    assert_eq!(list_users_status(&state, &token).await, StatusCode::OK);
}

#[actix_web::test]
async fn a_user_who_is_not_an_admin_gets_no_manage_token() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let state = state().await;
    let session = signed_in(&state, "guest").await;
    assert_eq!(manage_token(&session).await, Err("not_permitted"));
    let counted = crate::telemetry::counter(
        &snapshotter,
        "pharos_manage_token_issued_total",
        &[("outcome", "not_admin")],
    );
    assert_eq!(counted, 1, "the refusal is counted as not_admin");
}

#[actix_web::test]
async fn signing_out_ends_the_manage_token() {
    let state = state().await;
    let session = signed_in(&state, "boss").await;
    let (token, _) = manage_token(&session).await.unwrap();
    assert_eq!(list_users_status(&state, &token).await, StatusCode::OK);
    let mut req = session.logout_request();
    req.get().init_ctx();
    req.send().promise.await.unwrap();
    assert_eq!(
        list_users_status(&state, &token).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(manage_token(&session).await, Err("revoked"));
}

#[actix_web::test]
async fn the_device_list_shows_the_session_not_its_manage_tokens() {
    let state = state().await;
    let session = signed_in(&state, "boss").await;
    let (token, _) = manage_token(&session).await.unwrap();
    manage_token(&session).await.unwrap();
    let app = test::init_service(build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Devices")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.contains("pharos-native:manage"), "{text}");
    assert!(text.contains("\"TotalRecordCount\":1"), "{text}");
}
