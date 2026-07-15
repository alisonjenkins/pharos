#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

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
    let token = stores.issue(uid, "test").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
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
async fn sessions_empty_on_fresh_state() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Sessions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    assert_eq!(std::str::from_utf8(&body).unwrap(), "[]");
}

#[actix_web::test]
async fn playing_then_sessions_lists_active() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    let playing = test::TestRequest::post()
        .uri("/Sessions/Playing")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "sess-1",
            "PositionTicks": 0u64
        }))
        .to_request();
    let resp = test::call_service(&app, playing).await;
    assert_eq!(resp.status(), 204);

    let list = test::TestRequest::get()
        .uri("/Sessions")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, list).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["NowPlayingItemId"], "100");
    assert_eq!(arr[0]["Id"], "sess-1");
    // The dashboard "Active Devices" panel formats these with date-fns; a
    // missing value → `new Date(undefined)` → a fatal "Invalid time value"
    // that crashes the whole dashboard landing page. Must be a non-empty
    // ISO8601 string.
    let last = arr[0]["LastActivityDate"].as_str().unwrap_or("");
    assert!(
        last.starts_with("20") && last.contains('T'),
        "LastActivityDate must be ISO8601, got {last:?}"
    );
    assert!(
        arr[0]["LastPlaybackCheckIn"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "LastPlaybackCheckIn must be present"
    );
}

#[actix_web::test]
async fn progress_updates_position_and_pause() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    let playing = test::TestRequest::post()
        .uri("/Sessions/Playing")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "s1",
            "PositionTicks": 0u64
        }))
        .to_request();
    test::call_service(&app, playing).await;

    let progress = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "100",
            "PlaySessionId": "s1",
            "PositionTicks": 123_456_789u64,
            "IsPaused": true
        }))
        .to_request();
    test::call_service(&app, progress).await;

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v[0]["PositionTicks"], 123_456_789u64);
    assert_eq!(v[0]["IsPaused"], true);
}

#[actix_web::test]
async fn stopped_removes_session() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "ItemId": "100", "PlaySessionId":"s1", "PositionTicks": 0u64
            }))
            .to_request(),
    )
    .await;
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({ "PlaySessionId": "s1" }))
            .to_request(),
    )
    .await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(std::str::from_utf8(&body).unwrap(), "[]");
}

#[actix_web::test]
async fn capabilities_then_sessions_reflects_caps() {
    // P39 — round-trip check. POST /Sessions/Capabilities must
    // persist into the snapshot the GET /Sessions handler returns,
    // and the JSON wire shape must use Jellyfin's PascalCase field
    // names so jellyfin-web's remote-control UI greys the right
    // buttons.
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    // Start a session so the SetCapabilities event has a record to
    // mutate alongside the stub-insert fallback.
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "ItemId": "100", "PlaySessionId":"sess-cap", "PositionTicks": 0u64
            }))
            .to_request(),
    )
    .await;

    let caps = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Capabilities")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "Id": "sess-cap",
                "PlayableMediaTypes": ["Video", "Audio"],
                "SupportedCommands": ["VolumeUp", "VolumeDown", "Pause"],
                "MaxStreamingBitrate": 8_000_000u64,
                "SupportsMediaControl": true
            }))
            .to_request(),
    )
    .await;
    assert_eq!(caps.status(), 204);

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entry = v
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["Id"] == "sess-cap")
        .expect("session sess-cap missing from /Sessions snapshot");
    assert_eq!(
        entry["PlayableMediaTypes"],
        serde_json::json!(["Video", "Audio"])
    );
    assert_eq!(
        entry["SupportedCommands"],
        serde_json::json!(["VolumeUp", "VolumeDown", "Pause"])
    );
    assert_eq!(entry["MaxStreamingBitrate"], 8_000_000u64);
    assert_eq!(entry["SupportsMediaControl"], true);
}

#[actix_web::test]
async fn capabilities_accepts_body_and_returns_204() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Capabilities")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "PlayableMediaTypes": ["Video","Audio"],
                "SupportedCommands": ["VolumeUp","VolumeDown","PlayState"]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
}

#[actix_web::test]
async fn sessions_requires_auth() {
    let (state, _t) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/Sessions").to_request()).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn playstate_command_broadcasts_session_command() {
    let (state, token) = seed().await;
    // Subscribe before firing so we observe the broadcast.
    let mut rx = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::post()
        .uri("/Sessions/sess-42/Playing/Pause")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let evt = rx.recv().await.expect("bus delivered SessionCommand");
    match evt {
        pharos_server::state::SocketBroadcast::SessionCommand {
            session_id,
            command,
            ..
        } => {
            assert_eq!(session_id, "sess-42");
            assert_eq!(command, "Pause");
        }
        other => panic!("expected SessionCommand, got {other:?}"),
    }
}

#[actix_web::test]
async fn seek_command_carries_position_ticks_in_arg() {
    let (state, token) = seed().await;
    let mut rx = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::post()
        .uri("/Sessions/sess-42/Playing/Seek")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({ "SeekPositionTicks": 50_000_000u64 }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let evt = rx.recv().await.unwrap();
    if let pharos_server::state::SocketBroadcast::SessionCommand { arg, command, .. } = evt {
        assert_eq!(command, "Seek");
        assert_eq!(arg["SeekPositionTicks"], 50_000_000u64);
    } else {
        panic!("expected SessionCommand");
    }
}

/// Playback reports must nudge the trickplay priority channel with the parsed
/// item id — for BOTH the start and progress paths, and for BOTH wire id
/// forms (canonical 32-hex + legacy decimal). This is what lets the
/// pre-generator build the currently-watched episode's scrub previews
/// mid-session (and re-learn what's playing after a pod restart, when
/// PlaybackInfo was served by a previous process).
#[actix_web::test]
async fn playback_reports_nudge_trickplay_priority() {
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
    let token = stores.issue(uid, "test").await.unwrap();
    let token = token.0.expose().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let state = web::Data::new(AppState::new(stores, "t".into()).with_trickplay_priority(tx));
    let app = test::init_service(build_app(state)).await;

    // Start: canonical hex id.
    let hex = format!("{:032x}", 77u64);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "ItemId": hex, "PlaySessionId": "s1", "PositionTicks": 0u64
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    assert_eq!(rx.try_recv().ok(), Some(77), "start must nudge with hex id");

    // Progress: legacy decimal id.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(("X-Emby-Token", token.as_str()))
            .set_json(serde_json::json!({
                "ItemId": "78", "PlaySessionId": "s1",
                "PositionTicks": 10_000_000u64, "IsPaused": false
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        rx.try_recv().ok(),
        Some(78),
        "progress must nudge with decimal id"
    );
}

// B65 — the Android/Google-TV app POSTs /Sessions/Capabilities with its
// capabilities as QUERY params and NO body. A required web::Json 400'd it,
// which crashed the TV ("Something went wrong") right after a successful Quick
// Connect login. The body-less query form must succeed (204).
#[actix_web::test]
async fn capabilities_query_form_no_body_returns_204() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Sessions/Capabilities?playableMediaTypes=Video&playableMediaTypes=Audio&supportedCommands=MoveUp&supportedCommands=MoveDown&supportsMediaControl=true&supportsPersistentIdentifier=true")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        204,
        "body-less query-param Capabilities must not 400 (TV crash)"
    );
}

// The JSON-body form (/Sessions/Capabilities/Full) must still work.
#[actix_web::test]
async fn capabilities_full_json_body_still_returns_204() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Sessions/Capabilities/Full")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"PlayableMediaTypes":["Video"],"SupportedCommands":["MoveUp"],"SupportsMediaControl":true}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204, "JSON-body Capabilities/Full must work");
}

// B65 — the path-less /UserItems/Resume alias (Android TV) must resolve to the
// resume list, not 404 (which left the TV home "Continue Watching" broken).
#[actix_web::test]
async fn user_items_resume_alias_returns_200() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/UserItems/Resume?limit=10")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/UserItems/Resume alias must not 404");
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/UserItems/Resume")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["Items"].is_array(), "resume result carries Items array");
}
