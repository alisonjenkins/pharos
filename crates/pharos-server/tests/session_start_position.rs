//! The ROUTE, not just the helper, must report where a play session started.
//!
//! `session_start.rs`'s own tests prove the classifier and the once-per-session
//! rule. They stay green if every call site is deleted — and the call site is
//! the entire point: a player aimed off the end of the media is visible only in
//! the FIRST segment index it asks for, and only the segment handlers see that.
//!
//! Deliberately ffmpeg-free. The report fires before any transcode, so the
//! item can point at a path that does not exist and the request can fail: what
//! is under test is that the handler said where the session started, not that
//! it produced bytes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::hls,
    auth::BuiltinAuth,
    state::{AppState, Stores},
    transcode_sessions::TranscodeSession,
};

/// 60 s at the 6 s grid = 10 segments, so the last index is 9. Ant-Man's real
/// shape (1171 of 1172) scaled down; what matters is that the tail index is
/// not also index 0.
const DURATION_MS: u64 = 60_000;
const LAST_SEG: u32 = 9;

async fn seed() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:")
        .await
        .expect("open store");
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth
        .hash_password(&SecretString::new("p"))
        .expect("hash password");
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .expect("create user");
    let token = stores.issue(uid, "t").await.expect("issue token");
    stores
        .put(MediaItem {
            id: 7,
            path: "/nonexistent/never-decoded.mkv".into(),
            title: "probe".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(DURATION_MS),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("put item");
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

/// Register a live play session, since the segment handlers 410 an unknown
/// `PlaySessionId` before they reach the report.
async fn register_session(state: &AppState, psid: &str) {
    state
        .transcode_sessions
        .insert(
            psid.into(),
            TranscodeSession {
                media_id: 7,
                decision: pharos_server::api::jellyfin::device_profile::Decision::Transcode {
                    target_container: "mp4".into(),
                    target_video_codec: Some("h264".into()),
                    target_audio_codec: Some("aac".into()),
                    max_video_bitrate_bps: Some(500_000),
                },
                source_probe: MediaProbe {
                    duration_ms: Some(DURATION_MS),
                    ..Default::default()
                },
                burn_subtitle_indices: Default::default(),
            },
        )
        .await
        .expect("insert session");
}

/// The incident shape: a session whose very first request is the final segment
/// of the grid. On 2026-08-15 seven consecutive sessions did exactly this and
/// the server recorded only that segment 1171 had failed — never that anyone
/// had STARTED there, which is the difference between a broken segment and a
/// player aimed past the end of the media.
#[actix_web::test]
async fn a_session_that_starts_on_the_last_segment_is_reported_as_a_tail_start() {
    let _ = pharos_server::obs::init("info", None);
    let (state, token) = seed().await;
    register_session(&state, "psid-tail").await;
    register_session(&state, "psid-head").await;
    let app = test::init_service(App::new().app_data(state.clone()).configure(hls::register)).await;

    // Status is irrelevant — there is no decodable source behind this item.
    let _ = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/videos/7/h264cmaf/{LAST_SEG}.m4s?PlaySessionId=psid-tail&api_key={token}"
            ))
            .to_request(),
    )
    .await;
    let _ = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/videos/7/h264cmaf/0.m4s?PlaySessionId=psid-head&api_key={token}"
            ))
            .to_request(),
    )
    .await;

    let body = pharos_server::obs::render();
    let starts: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("pharos_hls_session_start_total"))
        .collect();
    assert!(
        starts.iter().any(|l| l.contains("position=\"tail\"")
            && l.contains("surface=\"h264cmaf\"")
            && l.ends_with(" 1")),
        "a session starting on the final segment must be reported as a tail \
         start; got {starts:?}"
    );
    assert!(
        starts.iter().any(|l| l.contains("position=\"head\"")),
        "an ordinary start must still be reported, so a tail start is a \
         comparison and not an isolated fact; got {starts:?}"
    );
}

/// T126 — the ROUTE must report a segment one session keeps re-fetching.
///
/// `session_start.rs`'s own tests prove the counter and the report-once rule,
/// and stay green if every call site is deleted. The call site is the point: on
/// 2026-08-16 a wedged player pulled segment 1188 152 times and segment 929
/// 118 times while every server-side signal read green — all 200s, warm cache,
/// no errors. Nothing counted the repeats, so the failure was invisible for
/// fifteen minutes.
#[actix_web::test]
async fn a_session_refetching_one_segment_is_reported() {
    let _ = pharos_server::obs::init("info", None);
    let (state, token) = seed().await;
    register_session(&state, "psid-spin").await;
    register_session(&state, "psid-walk").await;
    let app = test::init_service(App::new().app_data(state.clone()).configure(hls::register)).await;

    // The wedge: one session, one index, over and over. Status is irrelevant —
    // there is no decodable source behind this item, and what is under test is
    // that the server SAID a client was spinning.
    for _ in 0..8 {
        let _ = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/videos/7/h264cmaf/3.m4s?PlaySessionId=psid-spin&api_key={token}"
                ))
                .to_request(),
        )
        .await;
    }
    // …and a healthy session walking forward, which must NOT be reported.
    for seg in 0..8u32 {
        let _ = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/videos/7/h264cmaf/{seg}.m4s?PlaySessionId=psid-walk&api_key={token}"
                ))
                .to_request(),
        )
        .await;
    }

    let body = pharos_server::obs::render();
    let refetch: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("pharos_segment_refetch_total"))
        .collect();
    assert!(
        refetch
            .iter()
            .any(|l| l.contains("surface=\"h264cmaf\"") && l.ends_with(" 1")),
        "a session re-fetching one segment eight times must be reported exactly \
         once; got {refetch:?}"
    );
}
