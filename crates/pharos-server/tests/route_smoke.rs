//! Route-smoke test — hits every Jellyfin HTTP endpoint pharos
//! registers with reasonable default input + asserts none return
//! 5xx. Companion to `wire_invariants.rs`:
//!
//! - `wire_invariants` catches cross-field semantic bugs (DTOs that
//!   contradict each other after a real client touches them).
//! - `route_smoke` catches handler crashes on the empty / default
//!   shape of every endpoint — the surface a real user touches when
//!   nothing's seeded yet, plus the surface schemathesis-style
//!   fuzzers stress.
//!
//! Endpoints that need a non-default body (PlaybackInfo with empty
//! `{}` is fine but DisplayPreferences POST needs JSON) get a
//! minimal valid payload. Anything 4xx (auth, validation) is allowed
//! — only 5xx fails the test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
    sync::GroupRegistry,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (
    web::Data<AppState>,
    web::Data<GroupRegistry>,
    String,
    UserId,
) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "smoke".into()));
    let reg = web::Data::new(GroupRegistry::spawn());
    (state, reg, token.0.expose().to_string(), uid)
}

fn build_app(
    state: web::Data<AppState>,
    reg: web::Data<GroupRegistry>,
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
        .app_data(reg)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

/// Every endpoint we hit. `body` is `None` for GET, `Some` for POST.
struct Probe {
    method: &'static str,
    path: String,
    body: Option<&'static str>,
}

fn probes(user_id: &str) -> Vec<Probe> {
    let g = |path: &str| Probe {
        method: "GET",
        path: path.to_string(),
        body: None,
    };
    let p = |path: &str, body: &'static str| Probe {
        method: "POST",
        path: path.to_string(),
        body: Some(body),
    };
    let u = user_id;
    vec![
        // System
        g("/System/Info"),
        g("/System/Info/Public"),
        g("/System/Configuration"),
        g("/System/Endpoint"),
        // Users + branding
        g("/Users/Me"),
        g("/Users/Public"),
        g("/QuickConnect/Enabled"),
        g("/Branding/Configuration"),
        g("/Branding/Css"),
        // Library + items
        g("/Items"),
        g("/Items?IncludeItemTypes=Movie&Limit=10"),
        g(&format!("/Users/{u}/Items?Limit=5")),
        g(&format!("/Users/{u}/Items/Latest?Limit=5")),
        g(&format!("/Users/{u}/Items/Resume?Limit=5")),
        g(&format!("/Users/{u}/Views")),
        g("/UserViews"),
        g("/Library/VirtualFolders"),
        g("/Library/MediaFolders"),
        g("/Genres"),
        g("/Studios"),
        g("/Persons"),
        g("/Shows/NextUp?Limit=5"),
        g("/Shows/Upcoming"),
        // Search
        g("/Search/Hints?searchTerm=abc"),
        g("/Search/Suggestions"),
        g(&format!("/Users/{u}/Suggestions")),
        // Sessions
        g("/Sessions"),
        p(
            "/Sessions/Playing",
            r#"{"ItemId":"1","PlaySessionId":"s1","PositionTicks":0}"#,
        ),
        p(
            "/Sessions/Playing/Progress",
            r#"{"ItemId":"1","PlaySessionId":"s1","PositionTicks":1000,"IsPaused":false}"#,
        ),
        p("/Sessions/Playing/Stopped", r#"{"PlaySessionId":"s1"}"#),
        p("/Sessions/Capabilities", r#"{}"#),
        p("/Sessions/Capabilities/Full", r#"{}"#),
        p("/Sessions/sess-1/Playing/Pause", r#"{}"#),
        p(
            "/Sessions/sess-1/Playing/Seek",
            r#"{"SeekPositionTicks":1234}"#,
        ),
        p("/Sessions/sess-1/Command/DisplayContent", r#"{}"#),
        p("/Sessions/sess-1/Playing", r#"{"ItemIds":["1"]}"#),
        // SyncPlay
        g("/SyncPlay/List"),
        p("/SyncPlay/New", r#"{}"#),
        p("/SyncPlay/Join", r#"{}"#),
        p("/SyncPlay/Leave", r#"{}"#),
        p("/SyncPlay/Pause", r#"{}"#),
        p("/SyncPlay/Unpause", r#"{}"#),
        p("/SyncPlay/Seek", r#"{}"#),
        p("/SyncPlay/Ping", r#"{}"#),
        // LiveTV
        g("/LiveTv/Info"),
        g("/LiveTv/Channels"),
        g("/LiveTv/Recordings"),
        g("/LiveTv/Timers"),
        g("/LiveTv/SeriesTimers"),
        g("/LiveTv/TunerHosts"),
        // Preferences
        g("/DisplayPreferences/usersettings"),
        p(
            "/DisplayPreferences/home?client=emby",
            r#"{"ViewType":"poster"}"#,
        ),
        p(&format!("/Users/{u}/Configuration"), r#"{}"#),
        // Admin (admin user seeded)
        g("/Users"),
        g("/ScheduledTasks"),
        g("/Plugins"),
        g("/System/Logs"),
        g("/System/ActivityLog/Entries"),
        p("/Library/Refresh", r#"{}"#),
        // Playback
        g("/Playback/BitrateTest?Size=1000"),
    ]
}

#[actix_web::test]
async fn every_route_returns_under_500() {
    let (state, reg, token, uid) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    let mut failures: Vec<String> = Vec::new();

    let uid_str = uid.0.simple().to_string();
    for probe in probes(&uid_str) {
        let mut req = match probe.method {
            "GET" => test::TestRequest::get().uri(&probe.path),
            "POST" => test::TestRequest::post().uri(&probe.path),
            _ => unreachable!(),
        }
        .insert_header(("X-Emby-Token", token.as_str()));
        if let Some(body) = probe.body {
            req = req
                .insert_header(("content-type", "application/json"))
                .set_payload(body);
        }
        let resp = test::call_service(&app, req.to_request()).await;
        let status = resp.status().as_u16();
        if status >= 500 {
            failures.push(format!("{} {} → {}", probe.method, probe.path, status));
        }
    }
    assert!(
        failures.is_empty(),
        "endpoints returned 5xx (handler crashed on empty input):\n  {}",
        failures.join("\n  ")
    );
}
