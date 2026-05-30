#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /LiveTv endpoints — Channels list, single channel, stream redirect,
//! Programs EPG (T47).

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_discovery::live_tv::M3uXmltvBackend;
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use tempfile::TempDir;

const SAMPLE_M3U: &str = r#"#EXTM3U
#EXTINF:-1 tvg-id="bbc1" tvg-logo="https://example/bbc.png" tvg-chno="1" group-title="UK",BBC One
http://example/bbc1.ts
#EXTINF:-1 tvg-id="cnn" group-title="News",CNN
http://example/cnn.ts
"#;

const SAMPLE_XMLTV: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="bbc1"><display-name>BBC One</display-name></channel>
  <programme channel="bbc1" start="20240101100000 +0000" stop="20240101110000 +0000">
    <title>News at Ten</title>
    <desc>Top stories.</desc>
  </programme>
</tv>
"#;

async fn seed_state_with_live_tv() -> (web::Data<AppState>, String, TempDir) {
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
    let token = stores.issue(uid, "t").await.unwrap();

    let td = TempDir::new().unwrap();
    let m3u_path = td.path().join("p.m3u");
    let xml_path = td.path().join("epg.xml");
    tokio::fs::write(&m3u_path, SAMPLE_M3U).await.unwrap();
    tokio::fs::write(&xml_path, SAMPLE_XMLTV).await.unwrap();
    let backend = M3uXmltvBackend::new();
    backend.load_m3u(&m3u_path).await.unwrap();
    backend.load_xmltv(&xml_path).await.unwrap();

    let state = web::Data::new(AppState::new(stores, "t".into()).with_live_tv(backend));
    (state, token.0.expose().to_string(), td)
}

async fn seed_state_no_live_tv() -> (web::Data<AppState>, String) {
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
async fn channels_returns_parsed_m3u_entries() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items[0]["Id"], "bbc1");
    assert_eq!(items[0]["Name"], "BBC One");
    assert_eq!(items[0]["ChannelNumber"], "1");
    assert_eq!(items[0]["Type"], "Channel");
    assert_eq!(items[1]["Id"], "cnn");
}

#[actix_web::test]
async fn single_channel_404_for_unknown_id() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels/nope")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn channel_stream_redirects_to_upstream_url() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels/bbc1/Stream")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 302);
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "http://example/bbc1.ts",
    );
}

#[actix_web::test]
async fn programs_in_window_returns_epg() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Programs?minStartDate=2024-01-01T00:00:00Z&maxEndDate=2024-01-02T00:00:00Z")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items[0]["Name"], "News at Ten");
    assert_eq!(items[0]["ChannelId"], "bbc1");
    assert_eq!(items[0]["StartDate"], "2024-01-01T10:00:00.000Z");
    assert_eq!(items[0]["EndDate"], "2024-01-01T11:00:00.000Z");
}

#[actix_web::test]
async fn info_reports_enabled_when_backend_present() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsEnabled"], true);
}

#[actix_web::test]
async fn info_reports_disabled_when_no_backend() {
    let (state, token) = seed_state_no_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsEnabled"], false);
}

#[actix_web::test]
async fn channels_empty_when_no_backend() {
    let (state, token) = seed_state_no_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);
}

#[actix_web::test]
async fn recordings_timers_seriestimers_return_empty() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    for path in [
        "/LiveTv/Recordings",
        "/LiveTv/Timers",
        "/LiveTv/SeriesTimers",
        "/LiveTv/TunerHosts",
    ] {
        let req = test::TestRequest::get()
            .uri(path)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["TotalRecordCount"], 0, "{path}");
    }
}

#[actix_web::test]
async fn channels_requires_auth() {
    let (state, _token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}
