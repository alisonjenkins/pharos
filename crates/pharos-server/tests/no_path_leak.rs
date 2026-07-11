//! V9 invariant: raw media file paths never appear in any client-
//! facing response body. Clients only see opaque numeric MediaIds.
//!
//! Drives every read endpoint with a media item seeded at a distinctive
//! path (`/m/PATH-CANARY-.../...mkv`) and scans the response bodies
//! for that byte sequence. A handler that accidentally serialised
//! `MediaItem.path` (eg. into a debug field or error message) trips
//! this immediately.
//!
//! Why isolated from the auth/token audits: V9 leakage is *content*
//! leakage at the DTO boundary, V8 leakage is *credential* leakage.
//! Different root causes — different test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const CANARY_PATH: &str = "/m/PATH-CANARY-49a1b9c0-pharos-v9/SECRET-FILENAME-do-not-leak.mkv";

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
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: CANARY_PATH.into(),
            title: "Sample".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                container: Some("matroska".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                duration_ms: Some(60_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "audit-device").await.unwrap();
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

const SCAN_ROUTES: &[&str] = &[
    "/Items",
    "/Items/1",
    "/Items/1/PlaybackInfo",
    "/Items/1/Similar",
    "/Items/Counts",
    "/Users/Me",
    "/UserViews",
    "/Library/MediaFolders",
    "/Library/VirtualFolders",
    "/Genres",
    "/Studios",
    "/Artists",
    "/Albums",
    "/Shows/NextUp",
    "/Search/Hints?searchTerm=Sample",
    "/Search/Suggestions",
    "/Sessions",
    "/Devices",
    "/MediaSegments/1",
];

#[actix_web::test]
async fn no_endpoint_leaks_raw_media_path_in_response_body() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let mut leaks: Vec<String> = Vec::new();

    for path in SCAN_ROUTES {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(path)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        // Match on the unique segment — guards against incidental
        // matches on `/m/` if some debug field used it as a prefix.
        if body
            .windows(b"PATH-CANARY".len())
            .any(|w| w == b"PATH-CANARY")
        {
            leaks.push((*path).to_string());
        }
    }

    assert!(
        leaks.is_empty(),
        "V9 violation — raw media path appears in response body for: {leaks:?}"
    );
}
