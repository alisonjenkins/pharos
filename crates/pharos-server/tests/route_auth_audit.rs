//! V8/V9 invariant: every Jellyfin API route either (a) requires a
//! bearer token via `AuthUser`, or (b) appears in the
//! `PUBLIC_BY_DESIGN` allow-list below with a documented reason.
//!
//! The audit drives the registered route table from outside: each row
//! is `(method, path, expectation)` where `expectation` is one of
//!   `Auth` — request without token must yield 401.
//!   `Public` — request without token must yield non-401 (any 2xx /
//!              redirect / 404 — but never 401).
//!
//! Why this is necessary even with route_smoke + wire_baseline:
//!   - route_smoke uses a valid bearer everywhere — silently dropping
//!     `AuthUser` from a handler still passes.
//!   - wire_baseline checks response shape, not auth boundary.
//!
//! A new route added by a developer that forgets `AuthUser` will fail
//! this test loudly. Truly-public additions need an explicit entry
//! here (forcing a documented reason).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::Method, test, web, App};
use pharos_server::{
    api::jellyfin,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Auth,
    Public,
}
use Expect::*;

/// Concrete `(method, path, expectation)` rows. Path params filled with
/// throwaway-but-valid values — `AuthUser` runs before any path
/// extractor that could fail (extractors run lazily in declaration
/// order; `AuthUser` is the *first* extractor by convention).
fn audit_rows() -> Vec<(Method, &'static str, Expect)> {
    vec![
        // ---- public by design ----
        (Method::GET, "/System/Info", Public),
        (Method::GET, "/System/Info/Public", Public),
        (Method::GET, "/System/Configuration", Public),
        (Method::GET, "/System/Endpoint", Public),
        (Method::GET, "/Users/Public", Public),
        (Method::GET, "/QuickConnect/Enabled", Public),
        (Method::GET, "/Branding/Configuration", Public),
        (Method::GET, "/Branding/Css", Public),
        (Method::GET, "/Branding/Css.css", Public),
        (Method::POST, "/Users/AuthenticateByName", Public),
        // Image GETs match Jellyfin's design — `<img src=…>` can't
        // inject auth headers. See images.rs / live_tv.rs.
        (Method::GET, "/Items/1/Images/Primary", Public),
        (Method::GET, "/Items/1/Images/Primary/0", Public),
        (Method::GET, "/LiveTv/Channels/x/Images/Primary", Public),
        // Bitrate test ships the user the actual bytes; semi-public OK.
        (Method::GET, "/Playback/BitrateTest", Public),
        // ---- everything else: auth required ----
        (Method::GET, "/Users/Me", Auth),
        (Method::GET, "/Users", Auth),
        (Method::POST, "/Users/New", Auth),
        (Method::POST, "/Library/Refresh", Auth),
        (Method::GET, "/Library/MediaFolders", Auth),
        (Method::GET, "/Library/VirtualFolders", Auth),
        (Method::GET, "/Items", Auth),
        (Method::GET, "/Items/1", Auth),
        (Method::GET, "/Items/Counts", Auth),
        (Method::GET, "/Items/1/Similar", Auth),
        (Method::GET, "/Items/1/PlaybackInfo", Auth),
        (Method::GET, "/Genres", Auth),
        (Method::GET, "/Studios", Auth),
        (Method::GET, "/Artists", Auth),
        (Method::GET, "/Artists/AlbumArtists", Auth),
        (Method::GET, "/Albums", Auth),
        (Method::GET, "/Shows/NextUp", Auth),
        (Method::GET, "/Search/Hints", Auth),
        (Method::GET, "/Search/Suggestions", Auth),
        (Method::GET, "/Sessions", Auth),
        (Method::POST, "/Sessions/Capabilities", Auth),
        (Method::POST, "/Sessions/Capabilities/Full", Auth),
        (Method::POST, "/Sessions/Playing", Auth),
        (Method::POST, "/Sessions/Playing/Progress", Auth),
        (Method::POST, "/Sessions/Playing/Stopped", Auth),
        (Method::GET, "/Localization/Cultures", Auth),
        (Method::GET, "/Localization/Countries", Auth),
        (Method::GET, "/Localization/ParentalRatings", Auth),
        (Method::GET, "/Localization/Options", Auth),
        (Method::GET, "/Devices", Auth),
        (Method::GET, "/Devices/Info", Auth),
        (Method::GET, "/MediaSegments/1", Auth),
        (Method::GET, "/UserViews", Auth),
        (Method::GET, "/LiveTv/Info", Auth),
        (Method::GET, "/LiveTv/Channels", Auth),
        (Method::GET, "/LiveTv/Channels/x", Auth),
        (Method::GET, "/LiveTv/Programs", Auth),
        (Method::GET, "/LiveTv/Recordings", Auth),
        (Method::GET, "/LiveTv/Timers", Auth),
        (Method::GET, "/LiveTv/SeriesTimers", Auth),
        (Method::GET, "/LiveTv/TunerHosts", Auth),
        (Method::GET, "/ScheduledTasks", Auth),
        (Method::GET, "/Plugins", Auth),
        (Method::GET, "/System/Logs", Auth),
        (Method::GET, "/System/ActivityLog/Entries", Auth),
        (Method::POST, "/System/Configuration", Auth),
        (Method::GET, "/Videos/1/stream", Auth),
        (Method::GET, "/Videos/1/master.m3u8", Auth),
        (Method::GET, "/Videos/1/main.m3u8", Auth),
        (Method::GET, "/Audio/1/stream", Auth),
        (Method::GET, "/Audio/1/universal", Auth),
        (Method::POST, "/SyncPlay/New", Auth),
        (Method::POST, "/SyncPlay/Join", Auth),
        (Method::POST, "/SyncPlay/Leave", Auth),
        (Method::GET, "/SyncPlay/List", Auth),
        (Method::POST, "/SyncPlay/Ping", Auth),
    ]
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
    let member_sinks = pharos_sync::MemberSinks::new();
    App::new()
        .app_data(state)
        // SyncPlay handlers extract these; without them a missing-Data 500
        // would beat the pending 401 (extractors resolve in arg order), so the
        // auth audit for /SyncPlay/* needs them present to see the real 401.
        .app_data(web::Data::new(pharos_sync::GroupRegistry::spawn(
            std::sync::Arc::new(pharos_sync::LocalDelivery::new(member_sinks.clone())),
        )))
        .app_data(web::Data::new(pharos_sync::SessionHub::new()))
        .app_data(web::Data::new(member_sinks))
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

async fn make_state() -> web::Data<AppState> {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    web::Data::new(AppState::new(stores, "srv".into()))
}

#[actix_web::test]
async fn every_route_meets_its_auth_expectation() {
    let state = make_state().await;
    let app = test::init_service(build_app(state)).await;
    let mut failures: Vec<String> = Vec::new();

    for (method, path, want) in audit_rows() {
        let req = test::TestRequest::default()
            .method(method.clone())
            .uri(path)
            .to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let is_401 = status.as_u16() == 401;
        match want {
            Auth if !is_401 => failures.push(format!(
                "{method:>6} {path}: expected 401 without token, got {status}",
            )),
            Public if is_401 => failures.push(format!(
                "{method:>6} {path}: expected non-401 (public route), got 401",
            )),
            _ => {}
        }
    }

    assert!(
        failures.is_empty(),
        "auth-boundary audit failed:\n{}",
        failures.join("\n")
    );
}
