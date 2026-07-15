#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /QuickConnect/{Enabled,Initiate,Authorize,Connect} integration
//! flow against a real SqliteStore + AppState.

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// Deserialize an `/Users/AuthenticateWithQuickConnect` response body into
/// structs that mirror the jellyfin-sdk-kotlin models' NON-nullable properties.
/// Every field is required (no `Option`), so `serde` fails if pharos omits any
/// field the kotlin SDK marks non-null — the exact failure mode behind the
/// Android-TV "Unable to connect to server" (B63/B64). Fields the kotlin model
/// makes nullable or defaulted are simply not listed here (unknown fields are
/// ignored). Panics on a mismatch, with the serde error naming the field.
fn kotlin_strict_auth_result(body: &[u8]) {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct KtAuthResult {
        access_token: String,
        server_id: String,
        user: KtUser,
        session_info: KtSession,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct KtUser {
        id: String, // kotlin UUID; dashless-hex is accepted by the SDK serializer
        policy: KtPolicy,
        configuration: KtConfig,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct KtPolicy {
        is_administrator: bool,
        is_hidden: bool,
        is_disabled: bool,
        enable_user_preference_access: bool,
        enable_remote_control_of_other_users: bool,
        enable_shared_device_control: bool,
        enable_remote_access: bool,
        enable_live_tv_management: bool,
        enable_live_tv_access: bool,
        enable_media_playback: bool,
        enable_audio_playback_transcoding: bool,
        enable_video_playback_transcoding: bool,
        enable_playback_remuxing: bool,
        force_remote_source_transcoding: bool,
        enable_content_deletion: bool,
        enable_content_downloading: bool,
        enable_sync_transcoding: bool,
        enable_media_conversion: bool,
        enable_all_devices: bool,
        enable_all_channels: bool,
        enable_all_folders: bool,
        invalid_login_attempt_count: i64,
        login_attempts_before_lockout: i64,
        max_active_sessions: i64,
        enable_public_sharing: bool,
        remote_client_bitrate_limit: i64,
        authentication_provider_id: String,
        password_reset_provider_id: String,
        sync_play_access: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct KtConfig {
        play_default_audio_track: bool,
        display_missing_episodes: bool,
        grouped_folders: Vec<String>,
        subtitle_mode: String,
        display_collections_view: bool,
        enable_local_password: bool,
        ordered_views: Vec<String>,
        latest_items_excludes: Vec<String>,
        my_media_excludes: Vec<String>,
        hide_played_in_latest: bool,
        remember_audio_selections: bool,
        remember_subtitle_selections: bool,
        enable_next_episode_auto_play: bool,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct KtSession {
        user_id: String,
        last_activity_date: String,
        last_playback_check_in: String,
        is_active: bool,
        supports_media_control: bool,
        supports_remote_control: bool,
        has_custom_device_name: bool,
        playable_media_types: Vec<String>,
        supported_commands: Vec<String>,
    }
    if let Err(e) = serde_json::from_slice::<KtAuthResult>(body) {
        panic!(
            "AuthenticationResult is missing a kotlin-required (non-null) field: {e}\nbody: {}",
            String::from_utf8_lossy(body)
        );
    }
}

async fn seed_admin() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
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
async fn enabled_endpoint_returns_true() {
    let (state, _) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/QuickConnect/Enabled")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v, serde_json::Value::Bool(true));
}

#[actix_web::test]
async fn initiate_response_includes_device_and_app_metadata() {
    // The Jellyfin Android/Google TV app deserializes the QuickConnectResult
    // into a model with non-null DeviceName / AppName / AppVersion. Omitting
    // them makes the kotlin SDK reject the response → the app greys out the
    // Quick Connect button. They come from the `X-Emby-Authorization` header.
    let (state, _) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="Jellyfin Android TV", Device="Chromecast", DeviceId="dev-qc", Version="0.19.9""#,
        ))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["DeviceName"], "Chromecast", "DeviceName from header");
    assert_eq!(v["AppName"], "Jellyfin Android TV", "AppName = Client");
    assert_eq!(v["AppVersion"], "0.19.9", "AppVersion = Version");
    assert_eq!(v["DeviceId"], "dev-qc");
}

#[actix_web::test]
async fn connect_poll_includes_device_and_app_metadata() {
    // B61 — the TV shows the code from Initiate, then POLLS /QuickConnect/Connect
    // every ~5s. That poll response is ALSO a QuickConnectResult, so it must
    // carry the same non-null DeviceName/AppName/AppVersion; omitting them made
    // the kotlin SDK reject the poll → the app hid the code seconds after it
    // appeared (before the user could type it). Guards against the Connect
    // response drifting from Initiate's shape again.
    let (state, _) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="Jellyfin Android TV", Device="Chromecast", DeviceId="dev-qc", Version="0.19.9""#,
        ))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let secret = v["Secret"].as_str().unwrap().to_string();

    // The poll must echo the SAME metadata Initiate returned.
    let req = test::TestRequest::get()
        .uri(&format!("/QuickConnect/Connect?Secret={secret}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["DeviceName"], "Chromecast", "poll carries DeviceName");
    assert_eq!(v["AppName"], "Jellyfin Android TV", "poll carries AppName");
    assert_eq!(v["AppVersion"], "0.19.9", "poll carries AppVersion");
    // All three must be present as strings (a missing key fails kotlin decode).
    for k in ["DeviceName", "AppName", "AppVersion", "DateAdded"] {
        assert!(
            v[k].is_string(),
            "{k} must be a non-null string on the poll"
        );
    }
}

#[actix_web::test]
async fn full_flow_finalizes_at_authenticatewithquickconnect() {
    // The real jellyfin-web two-endpoint exchange: poll /QuickConnect/Connect
    // (read-only, echoes Secret) then finalize at
    // /Users/AuthenticateWithQuickConnect with that Secret to get the token.
    let (state, admin_token) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;

    // Step 1: Initiate (unauthenticated).
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="cli", Device="d", DeviceId="dev-qc", Version="1""#,
        ))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let code = v["Code"].as_str().unwrap().to_string();
    let secret = v["Secret"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 6);
    assert_eq!(v["Authenticated"], serde_json::Value::Bool(false));

    // Finalize BEFORE authorize → 401 (not yet vouched for).
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateWithQuickConnect")
        .insert_header(("content-type", "application/json"))
        .set_payload(format!(r#"{{"Secret":"{secret}"}}"#))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "finalize before authorize must 401");

    // Step 2: Authorize (admin bearer).
    let req = test::TestRequest::post()
        .uri(&format!("/QuickConnect/Authorize?Code={code}"))
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 3: Connect poll — Authenticated:true, echoes Secret, NO token.
    // Read-only: poll twice, both succeed (must not consume).
    for _ in 0..2 {
        let req = test::TestRequest::get()
            .uri(&format!("/QuickConnect/Connect?Secret={secret}"))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["Authenticated"], serde_json::Value::Bool(true));
        assert_eq!(v["Secret"], secret, "Connect must echo Secret back");
        assert!(
            v.get("AccessToken").is_none() || v["AccessToken"].is_null(),
            "Connect must not mint a token"
        );
    }

    // Step 4: Finalize → AuthenticationResult with User.Id + AccessToken.
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateWithQuickConnect")
        .insert_header(("content-type", "application/json"))
        .set_payload(format!(r#"{{"Secret":"{secret}"}}"#))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tok = v["AccessToken"].as_str().unwrap();
    assert!(!tok.is_empty());
    assert!(v["User"]["Id"].as_str().is_some(), "result carries User.Id");
    // B63/B64 — replicate the jellyfin-sdk-kotlin (Android/Google TV) parser:
    // deserialize the finalize AuthenticationResult into structs whose fields
    // mirror the kotlin model's NON-nullable properties (no `?`). serde errors
    // on any missing one — exactly what made the TV throw "Unable to connect to
    // server" while the server had already issued the token. Any future DTO
    // that drops a kotlin-required field fails HERE instead of on a device.
    kotlin_strict_auth_result(&body);

    // The issued token actually authenticates.
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", tok))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "quick-connect token must authenticate");

    // Step 5: finalize is one-shot — a second exchange of the same secret 401s.
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateWithQuickConnect")
        .insert_header(("content-type", "application/json"))
        .set_payload(format!(r#"{{"Secret":"{secret}"}}"#))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "secret must be single-use at finalize");
}

#[actix_web::test]
async fn flow_works_with_lowercase_query_params_like_android_clients() {
    // Regression: the Jellyfin Android TV app polls `?secret=` and the mobile
    // browser authorizes with `?code=` (lowercase — the real Jellyfin API
    // param casing), but pharos used to bind PascalCase-only and 400'd every
    // request, so the login device "timed out before you could enter the code".
    let (state, admin_token) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="Jellyfin Android TV", Device="d", DeviceId="dev-qc", Version="1""#,
        ))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let code = v["Code"].as_str().unwrap().to_string();
    let secret = v["Secret"].as_str().unwrap().to_string();

    // Poll with LOWERCASE `secret` (Android TV) → 200, not 400.
    let req = test::TestRequest::get()
        .uri(&format!("/QuickConnect/Connect?secret={secret}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "lowercase ?secret= must bind");

    // Authorize with LOWERCASE `code` + an extra `userId` param (as the mobile
    // browser sends) → 200, not 400.
    let req = test::TestRequest::post()
        .uri(&format!(
            "/QuickConnect/Authorize?code={code}&userId=abc123"
        ))
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "lowercase ?code= must bind");

    // Finalize with a lowercase-keyed JSON body → 200 + token.
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateWithQuickConnect")
        .insert_header(("content-type", "application/json"))
        .set_payload(format!(r#"{{"secret":"{secret}"}}"#))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["AccessToken"].as_str().is_some_and(|t| !t.is_empty()),
        "lowercase JSON body {{\"secret\"}} must finalize; got {v}"
    );
}

#[actix_web::test]
async fn authorize_unknown_code_404s() {
    let (state, admin_token) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Authorize?Code=999999")
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn authorize_requires_auth() {
    let (state, _) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Authorize?Code=000000")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_client_error(), "{}", resp.status());
}

#[actix_web::test]
async fn pending_connect_returns_authenticated_false() {
    let (state, _) = seed_admin().await;
    let app = test::init_service(build_app(state)).await;
    // Initiate.
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="cli", Device="d", DeviceId="dev-qc", Version="1""#,
        ))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let secret = v["Secret"].as_str().unwrap().to_string();

    // Connect immediately — pending, no token.
    let req = test::TestRequest::get()
        .uri(&format!("/QuickConnect/Connect?Secret={secret}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Authenticated"], serde_json::Value::Bool(false));
    assert!(v.get("AccessToken").is_none() || v["AccessToken"].is_null());
}
