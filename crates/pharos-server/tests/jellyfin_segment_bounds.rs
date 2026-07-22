#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Segment-index bounds checks (seek hardening).
//!
//! A client can request a segment index past the end of the VOD playlist —
//! from a stale playlist, a client bug, or a probe duration that overshoots the
//! real media. That used to reach the transcoder: the h264 path cached an
//! empty-tail segment and served it as a 200 forever; the VP9 path produced no
//! frames and surfaced a NoMoof/NoMoov → 500. Both must be a clean 404.
//!
//! The guard runs BEFORE any transcode, so these assertions need no ffmpeg —
//! an over-index 404s without touching the (here nonexistent) media file.

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

// 15 s duration @ 24 fps → ceil(15/6) = 3 segments: valid indices 0,1,2.
const DURATION_MS: u64 = 15_000;
const VALID_LAST: u32 = 2;
const OVER_INDEX: u32 = 3;

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
    let token = stores.issue(uid, "t").await.unwrap();
    stores
        .put(MediaItem {
            id: 42,
            // No real file — the bounds guard 404s before any transcode reads it.
            path: "/nonexistent/clip.webm".into(),
            title: "clip".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(DURATION_MS),
                frame_rate_mille: Some(24_000),
                video_codec: Some("vp9".into()),
                container: Some("webm".into()),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

macro_rules! build_app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state)
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

macro_rules! get_status {
    ($app:expr, $token:expr, $uri:expr) => {{
        let req = test::TestRequest::get()
            .uri(&format!("{}?api_key={}", $uri, $token))
            .to_request();
        test::call_service(&$app, req).await.status().as_u16()
    }};
}

#[actix_web::test]
async fn h264_segment_over_index_is_404() {
    let (state, token) = seed().await;
    let app = build_app!(state);
    let status = get_status!(app, token, format!("/videos/42/hls1/main/{OVER_INDEX}.ts"));
    assert_eq!(
        status, 404,
        "a segment past the VOD grid must be 404, not an empty-tail 200"
    );
}

#[actix_web::test]
async fn vp9_segment_over_index_is_404() {
    let (state, token) = seed().await;
    let app = build_app!(state);
    let status = get_status!(app, token, format!("/videos/42/vp9/{OVER_INDEX}.m4s"));
    assert_eq!(
        status, 404,
        "a VP9 segment past the VOD grid must be 404, not a NoMoov 500"
    );
}

// The guard must not OVER-reject: the last in-bounds segment passes the check
// and proceeds (here it fails downstream on the missing file — any status other
// than the bounds 404 proves the guard let it through).
#[actix_web::test]
async fn last_valid_segment_passes_the_bounds_guard() {
    let (state, token) = seed().await;
    let app = build_app!(state);
    let h264 = get_status!(app, token, format!("/videos/42/hls1/main/{VALID_LAST}.ts"));
    assert_ne!(
        h264, 404,
        "the last valid h264 segment must clear the bounds guard (got {h264})"
    );
    let vp9 = get_status!(app, token, format!("/videos/42/vp9/{VALID_LAST}.m4s"));
    assert_ne!(
        vp9, 404,
        "the last valid VP9 segment must clear the bounds guard (got {vp9})"
    );
}
