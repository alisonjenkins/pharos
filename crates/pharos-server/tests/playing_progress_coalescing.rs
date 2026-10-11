#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Progress reports are written to the database at most once per interval per
//! play session, except where waiting would lose something: the stop, a pause
//! flip and a seek. The session registry is updated by every report either
//! way; only the database write is held back.
//!
//! Writes are counted from the `record_playback_progress` store span, so the
//! assertions are about database work, not about a helper's internals.

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
use tracing_subscriber::prelude::*;

#[path = "common/store_calls.rs"]
mod store_calls;
use store_calls::StoreCallLog;

const SEC: u64 = 10_000_000;
/// 60 s of media, so the 90% played threshold sits at 54 s.
const RUNTIME_MS: u64 = 60_000;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    seed_with(None).await
}

async fn seed_with(interval: Option<std::time::Duration>) -> (web::Data<AppState>, String, UserId) {
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
                duration_ms: Some(RUNTIME_MS),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let mut state = AppState::new(stores, "srv".into());
    if let Some(i) = interval {
        state = state.with_progress_persist_interval(i);
    }
    let state = web::Data::new(state);
    (state, token.0.expose().to_string(), uid)
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

macro_rules! post {
    ($app:expr, $token:expr, $uri:expr, $body:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("X-Emby-Token", $token.as_str()))
                .set_json($body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 204, "{}", $uri);
    }};
}

macro_rules! progress {
    ($app:expr, $token:expr, $secs:expr) => {
        progress!($app, $token, $secs, false)
    };
    ($app:expr, $token:expr, $secs:expr, $paused:expr) => {
        post!(
            $app,
            $token,
            "/Sessions/Playing/Progress",
            serde_json::json!({
                "ItemId": "1", "PlaySessionId": "s1",
                "PositionTicks": $secs * SEC, "IsPaused": $paused
            })
        )
    };
}

macro_rules! stopped {
    ($app:expr, $token:expr, $secs:expr) => {
        post!(
            $app,
            $token,
            "/Sessions/Playing/Stopped",
            serde_json::json!({
                "ItemId": "1", "PlaySessionId": "s1", "PositionTicks": $secs * SEC
            })
        )
    };
}

fn writes(log: &StoreCallLog) -> usize {
    log.take()
        .iter()
        .filter(|c| *c == "record_playback_progress")
        .count()
}

async fn stored(state: &web::Data<AppState>, uid: UserId) -> pharos_core::UserItemData {
    state.stores.get_user_data(uid, 1).await.unwrap()
}

#[actix_web::test]
async fn two_quick_ticks_write_once_but_the_registry_sees_both() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    post!(
        app,
        token,
        "/Sessions/Playing",
        serde_json::json!({"ItemId": "1", "PlaySessionId": "s1", "PositionTicks": 0u64})
    );
    let log = StoreCallLog::default();
    let _spans = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));

    progress!(app, token, 10u64);
    progress!(app, token, 11u64);

    assert_eq!(writes(&log), 1, "the second tick is held, not written");
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v[0]["PositionTicks"],
        11 * SEC,
        "now-playing follows every report"
    );
}

#[actix_web::test]
async fn stopping_stores_the_stop_position_not_the_last_written_tick() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    stopped!(app, token, 12u64);

    let ud = stored(&state, uid).await;
    assert_eq!(ud.last_played_position_ticks, 12 * SEC);
    assert!(!ud.played);
    assert!(
        state.progress_writes.take_all().is_empty(),
        "the stop leaves nothing held that could follow it"
    );
}

#[actix_web::test]
async fn pausing_writes_at_once() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );

    progress!(app, token, 12u64, true);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC
    );
    progress!(app, token, 13u64, true);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC,
        "ticks while paused are held again"
    );
    progress!(app, token, 13u64, false);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        13 * SEC,
        "unpausing writes at once too"
    );
}

#[actix_web::test]
async fn a_seek_writes_at_once() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 40u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        40 * SEC
    );
    progress!(app, token, 5u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        5 * SEC
    );
}

#[actix_web::test]
async fn the_watched_threshold_fires_once_however_many_ticks_preceded_the_stop() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    for secs in [50u64, 51, 52, 53, 55, 56] {
        progress!(app, token, secs);
    }
    stopped!(app, token, 57u64);

    let ud = stored(&state, uid).await;
    assert!(ud.played, "stopped past 90% of runtime marks played");
    assert_eq!(ud.play_count, 1, "played once, not once per tick");
    assert_eq!(
        ud.last_played_position_ticks, 0,
        "a finished item has no resume point"
    );
}

#[actix_web::test]
async fn a_zero_interval_writes_every_report() {
    let (state, token, uid) = seed_with(Some(std::time::Duration::ZERO)).await;
    let app = app!(state);
    let log = StoreCallLog::default();
    let _spans = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    progress!(app, token, 12u64);
    assert_eq!(writes(&log), 3);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC
    );
}
