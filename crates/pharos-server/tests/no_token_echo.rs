//! V8 invariant: bearer tokens never appear in response bodies.
//!
//! Drives every key Jellyfin endpoint with a deliberately
//! distinctive token (`AUDIT-CANARY-...`) and asserts the bytes never
//! show up in the response. A handler that accidentally echoes the
//! `X-Emby-Token` header back (eg. by serialising the raw
//! Authorization header into a debug field) trips this loudly.
//!
//! Why not just code-review for token logging? handlers grow new
//! debug fields, error messages with header dumps, etc.; a runtime
//! probe catches those before they ship.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const CANARY_DEVICE_ID: &str = "AUDIT-CANARY-DEVICE-49a1b9c0-pharos-v8";

async fn seed_with_canary_token() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/sample.mkv".into(),
            title: "Sample".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    // device_id IS part of /Devices output by design — V8 covers the
    // *token*, not the device label. We probe for the actual token
    // string the bearer carries.
    let token = stores.issue(uid, CANARY_DEVICE_ID).await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
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

/// Routes whose responses we scan for the bearer token. GETs only —
/// every one of these is read-only, so we don't risk side effects.
const SCAN_ROUTES: &[&str] = &[
    "/System/Info",
    "/System/Configuration",
    "/Users/Me",
    "/UserViews",
    "/Library/MediaFolders",
    "/Library/VirtualFolders",
    "/Items",
    "/Items/Counts",
    "/Items/1",
    "/Items/1/PlaybackInfo",
    "/Items/1/Similar",
    "/Genres",
    "/Studios",
    "/Artists",
    "/Albums",
    "/Shows/NextUp",
    "/Search/Hints?searchTerm=x",
    "/Search/Suggestions",
    "/Sessions",
    "/Devices",
    "/Devices/Info",
    "/Localization/Cultures",
    "/Localization/Countries",
    "/MediaSegments/1",
    "/ScheduledTasks",
    "/Plugins",
    "/System/Logs",
    "/System/ActivityLog/Entries",
];

#[actix_web::test]
async fn no_endpoint_echoes_bearer_token_in_response_body() {
    let (state, token) = seed_with_canary_token().await;
    let app = test::init_service(build_app(state)).await;
    let mut leaks: Vec<String> = Vec::new();

    // The ONE legitimate place the token may appear: as the value of an
    // `api_key=` query param inside a URL the client must fetch without an
    // auth header (the HLS `TranscodingUrl` for hls.js, which can't inject
    // headers — the same token the server already embeds in every variant
    // line of the master playlist). Every OTHER occurrence is a leak.
    let api_key_prefix = format!("api_key={token}");
    for path in SCAN_ROUTES {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(path)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let hay = body.as_ref();
        let needle = token.as_bytes();
        let legit = api_key_prefix.as_bytes();
        let mut i = 0;
        while let Some(off) = hay[i..].windows(needle.len()).position(|w| w == needle) {
            let at = i + off;
            // Is this occurrence the value right after `api_key=`?
            let ok = at >= (legit.len() - needle.len())
                && &hay[at - (legit.len() - needle.len())..at + needle.len()] == legit;
            if !ok {
                leaks.push((*path).to_string());
                break;
            }
            i = at + needle.len();
        }
    }

    assert!(
        leaks.is_empty(),
        "V8 violation — bearer token appears in response body for: {leaks:?}"
    );
}

/// Sanity check: the canary device_id IS expected to appear in
/// `/Devices` output (that's the whole point of that endpoint). This
/// proves the scan harness reaches the body — without this, a broken
/// harness might silently pass the leak test.
#[actix_web::test]
async fn devices_endpoint_does_surface_device_id_label() {
    let (state, token) = seed_with_canary_token().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Devices")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(
        s.contains(CANARY_DEVICE_ID),
        "/Devices missing seeded device label — scan harness broken? body={s}"
    );
}
