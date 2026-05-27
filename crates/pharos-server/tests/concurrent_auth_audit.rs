//! Concurrent /Users/AuthenticateByName must yield distinct tokens.
//!
//! UUID v4 collision is ~zero in theory, but pharos passes the
//! generated token through a UNIQUE constraint in auth_tokens. A
//! shared-state bug (eg. accidentally reusing a generated UUID
//! across handlers) would surface as a duplicate-token row + an
//! auth_tokens UNIQUE conflict, killing every concurrent login but
//! one.
//!
//! Drives 32 parallel logins against the same user, asserts:
//!   - Every login returned 200.
//!   - All tokens distinct.
//!   - tokens_for(user) reports the right count.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    AuthBackend, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::collections::HashSet;

async fn seed() -> (web::Data<AppState>, UserId) {
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
    // Sanity check: BuiltinAuth resolves the seeded credentials so
    // the parallel logins actually reach the issuance path.
    let _ = AuthBackend::authenticate(&auth, "u", &SecretString::new("p")).await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, uid)
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
async fn parallel_authenticate_by_name_yields_distinct_tokens() {
    let (state, uid) = seed().await;
    let app = std::rc::Rc::new(test::init_service(build_app(state.clone())).await);

    let n = 32usize;
    // futures::join_all polls all N requests cooperatively in this
    // task. actix runs each handler concurrently — sqlx awaits hit
    // the DB pool in interleaved order, which is enough to expose
    // any UNIQUE-constraint race in token issuance.
    let mut futs = Vec::with_capacity(n);
    for i in 0..n {
        let app = app.clone();
        futs.push(async move {
            let body = test::call_and_read_body(
                &*app,
                test::TestRequest::post()
                    .uri("/Users/AuthenticateByName")
                    .insert_header((
                        "X-Emby-Authorization",
                        format!(
                            r#"MediaBrowser Client="x", Device="d", DeviceId="dev-{i}", Version="1""#,
                        ),
                    ))
                    .insert_header(("content-type", "application/json"))
                    .set_payload(r#"{"Username":"u","Pw":"p"}"#)
                    .to_request(),
            )
            .await;
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            v["AccessToken"].as_str().unwrap_or("").to_string()
        });
    }
    let results: Vec<String> = futures_util::future::join_all(futs).await;

    let mut tokens: HashSet<String> = HashSet::new();
    for t in &results {
        assert!(!t.is_empty(), "every concurrent login must return a token");
        assert!(tokens.insert(t.clone()), "duplicate token across concurrent logins: {t}");
    }

    // All N tokens must show up in the auth_tokens table for this user.
    let rows = state.stores.tokens_for(uid).await.unwrap();
    assert_eq!(
        rows.len(),
        n,
        "tokens_for must report {n} issued tokens; got {}",
        rows.len()
    );
}
