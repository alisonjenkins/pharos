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
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserDataStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_with_runtime_ms(ms: u64) -> (web::Data<AppState>, String, UserId) {
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
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":570000000}"#)
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
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":300000000}"#)
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

#[actix_web::test]
async fn stop_near_end_broadcasts_full_played_dto() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, uid) = seed_with_runtime_ms(60_000).await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":570000000}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    // B36 — the frame must carry the full DTO (wire ItemId + Played) so
    // jellyfin-web flips the watched tick without a refresh.
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { user_id, entries } => {
            assert_eq!(user_id, uid.0.simple().to_string());
            assert_eq!(
                entries[0]["ItemId"],
                pharos_jellyfin_api::dto::wire_item_id(1)
            );
            assert_eq!(entries[0]["Played"], true);
            assert_eq!(entries[0]["PlayCount"], 1);
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
}

#[actix_web::test]
async fn progress_broadcasts_dto_with_played_percentage() {
    use pharos_server::state::SocketBroadcast;
    // 100s runtime = 1_000_000_000 ticks; report 50% in.
    let (state, token, uid) = seed_with_runtime_ms(100_000).await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"x","PositionTicks":500000000}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    // The progress fan-out may emit PlaybackProgress too — find the
    // UserDataChanged frame.
    let deadline = std::time::Duration::from_millis(500);
    let entry = loop {
        let msg = tokio::time::timeout(deadline, bus.recv())
            .await
            .expect("broadcast timeout")
            .expect("broadcast recv");
        if let SocketBroadcast::UserDataChanged { user_id, entries } = msg {
            assert_eq!(user_id, uid.0.simple().to_string());
            break entries.into_iter().next().expect("one entry");
        }
    };
    assert_eq!(entry["ItemId"], pharos_jellyfin_api::dto::wire_item_id(1));
    assert_eq!(entry["PlaybackPositionTicks"], 500000000u64);
    // B36 — PlayedPercentage computed from runtime drives the card
    // resume bar; a hardcoded 0 blanks it.
    let pct = entry["PlayedPercentage"].as_f64().expect("percentage");
    assert!((pct - 50.0).abs() < 0.1, "expected ~50%, got {pct}");
}
