//! `/Sessions/Playing/Stopped` UserData persistence contract.
//!
//! Two branches:
//!   1. Position within last 10% of runtime → item flips to played=true,
//!      play_count++, resume position reset.
//!   2. Position below the threshold → resume position saved as-is,
//!      played stays false.
//!
//! Without this, jellyfin-web's Resume row holds finished items
//! forever (the client only sends an explicit /PlayedItems POST on
//! manual mark-played).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserDataStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_with_runtime_ms(ms: u64) -> (web::Data<AppState>, String, UserId) {
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
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(ms),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string(), uid)
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
async fn stop_near_end_marks_played_and_resets_position() {
    // 60s runtime = 60_000ms = 600_000_000 ticks. 95% in = 570_000_000.
    let (state, token, uid) = seed_with_runtime_ms(60_000).await;
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(
                r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":570000000}"#,
            )
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let ud = state.stores.get_user_data(uid, 1).await.unwrap();
    assert!(ud.played, "near-end stop must mark played");
    assert_eq!(ud.play_count, 1, "play_count must increment");
    assert_eq!(
        ud.last_played_position_ticks, 0,
        "played item resets resume position"
    );
}

#[actix_web::test]
async fn stop_midway_saves_resume_position_but_not_played() {
    let (state, token, uid) = seed_with_runtime_ms(60_000).await;
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            // 50% in = 300_000_000 ticks. Well below the 90% cutoff.
            .set_payload(
                r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":300000000}"#,
            )
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let ud = state.stores.get_user_data(uid, 1).await.unwrap();
    assert!(!ud.played, "midway stop must NOT mark played");
    assert_eq!(
        ud.last_played_position_ticks, 300_000_000,
        "midway stop must save resume position"
    );
}

#[actix_web::test]
async fn stop_without_item_id_is_noop_for_user_data() {
    let (state, token, uid) = seed_with_runtime_ms(60_000).await;
    let app = test::init_service(build_app(state.clone())).await;
    // Seed prior UserData so we can assert it stays untouched.
    state
        .stores
        .set_user_data(
            uid,
            1,
            pharos_core::UserItemData {
                last_played_position_ticks: 12345,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"PlaySessionId":"x"}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let ud = state.stores.get_user_data(uid, 1).await.unwrap();
    assert_eq!(
        ud.last_played_position_ticks, 12345,
        "no ItemId in body must leave UserData untouched"
    );
}
