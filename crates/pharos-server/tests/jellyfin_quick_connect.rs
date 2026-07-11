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
async fn initiate_authorize_connect_full_flow_yields_access_token() {
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

    // Step 2: Authorize (admin bearer).
    let req = test::TestRequest::post()
        .uri(&format!("/QuickConnect/Authorize?Code={code}"))
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 3: Connect — returns AccessToken now.
    let req = test::TestRequest::get()
        .uri(&format!("/QuickConnect/Connect?Secret={secret}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Authenticated"], serde_json::Value::Bool(true));
    let tok = v["AccessToken"].as_str().unwrap();
    assert!(!tok.is_empty());

    // Second Connect returns 404 (one-shot consumed).
    let req = test::TestRequest::get()
        .uri(&format!("/QuickConnect/Connect?Secret={secret}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
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
