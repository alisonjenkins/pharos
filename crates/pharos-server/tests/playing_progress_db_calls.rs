#![allow(clippy::unwrap_used, clippy::expect_used)]
//! How many database calls one `POST /Sessions/Playing/Progress` costs.
//!
//! The route is the busiest one in production, so its store-call count is a
//! budget. Counted from the `#[tracing::instrument]` span every store method
//! opens (one span per round trip), so a regression shows up by name.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tracing_subscriber::prelude::*;

#[path = "common/store_calls.rs"]
mod store_calls;
use store_calls::StoreCallLog;

async fn seed() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
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
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_600_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
async fn a_progress_report_store_call_count() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let log = StoreCallLog::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));

    let report = |ticks: u64| {
        let req = test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(format!(
                r#"{{"ItemId":"1","PlaySessionId":"s","PositionTicks":{ticks}}}"#
            ))
            .to_request();
        test::call_service(&app, req)
    };

    assert_eq!(report(100_000_000).await.status().as_u16(), 204);
    assert_eq!(
        log.take(),
        ["resolve", "get", "get", "record_playback_progress"],
        "first report: token, user, item (kind and runtime), one user-data upsert"
    );
    assert_eq!(report(110_000_000).await.status().as_u16(), 204);
    assert_eq!(
        log.take(),
        ["resolve", "get", "get"],
        "a held report costs no user-data call"
    );
}
