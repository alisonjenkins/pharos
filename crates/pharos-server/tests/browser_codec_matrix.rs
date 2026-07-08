#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Browser × source-codec routing matrix.
//!
//! Codec negotiation for browsers is subtle and has been bug-prone, so this
//! pins the intended behaviour: given a realistic per-browser jellyfin-web
//! device profile + a source item, pharos must hand the browser a codec it can
//! actually decode. Each row asserts the resulting playback SURFACE:
//!
//! - `DirectPlay`  — SupportsDirectPlay=true (browser plays the file as-is).
//! - `HlsH264`     — H.264/mpegts HLS master (SubProtocol "hls").
//! - `WebmVp9`     — progressive VP9/WebM (SubProtocol "http").
//!
//! Ground-truth capabilities (researched against MDN "Web video codec guide" +
//! "Media container formats", caniuse.com, and Apple/Chromium docs — NOT the
//! codec-stripped Playwright builds, which under-report proprietary codecs):
//! - Chrome/Chromium: H.264 ✓; VP8/VP9 ✓; AV1 ✓ (v70+); HEVC conditional
//!   (v107+, needs an OS/HW decoder — no software fallback); AAC/Opus/Vorbis/
//!   FLAC/MP3 ✓; AC-3/E-AC-3 ✗ (disabled by default).
//! - Firefox (incl. Zen): VP8/VP9 ✓; AV1 ✓ (v67+); HEVC only v134+/HW-gated.
//!   **H.264 is UNRELIABLE**: Firefox bundles no licensed H.264 decoder, so on
//!   codec-less Linux it can't decode H.264 at all — yet `canPlayType` still
//!   reports "probably", so jellyfin-web lists H.264 and pharos can't tell the
//!   difference from the profile. A Firefox-UA quirk forces VP9/WebM for any
//!   non-VP source. AAC/MP3 have the same platform-codec fragility as H.264.
//! - Safari/WebKit: H.264 ✓; HEVC ✓ (v11+); VP8/VP9 conditional (14.1+/iOS15+);
//!   AV1 HW-only (v17+, M3/iPhone-15-Pro+); AAC ✓; AC-3/E-AC-3 ✓ (OS decode);
//!   plays HLS `.m3u8` NATIVELY in <video> (Chrome/Firefox need hls.js/MSE).
//! - Containers: NO browser plays raw Matroska/.mkv — WebM is a constrained
//!   subset with its own MIME. The SAME VP9/Opus bytes are accepted as
//!   `video/webm` but rejected as `video/x-matroska` (hence the mime rewrite
//!   in stream.rs). MPEG-TS is only consumed as HLS segments, never standalone.
//!
//! These per-browser profile fixtures are REPRESENTATIVE scenarios (e.g. the
//! Chrome fixture models a build WITHOUT HW HEVC), not the universal truth —
//! jellyfin-web builds the real profile from the actual browser's canPlayType,
//! and pharos negotiates off whatever profile it receives. The rows assert the
//! ROUTING is correct for the given profile + UA.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

const UA_CHROME: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const UA_FIREFOX: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0";
const UA_SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

/// Chrome/Chromium: H.264 + VP8/VP9/AV1 direct-play, H.264 HLS transcode.
const PROFILE_CHROME: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264,vp9,av1","AudioCodec":"aac,opus,mp3,flac"},
    {"Container":"webm","Type":"Video","VideoCodec":"vp8,vp9,av1","AudioCodec":"opus,vorbis"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
  ]}}"#;

/// Firefox/Zen: advertises H.264 (canPlayType lies) + VP-family direct-play,
/// AND a VP9/WebM transcode target. This is the buggy shape the quirk targets.
const PROFILE_FIREFOX: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264,vp9,av1","AudioCodec":"aac,flac,mp3"},
    {"Container":"webm","Type":"Video","VideoCodec":"vp8,vp9,av1","AudioCodec":"opus,vorbis"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"},
    {"Container":"webm","Type":"Video","Protocol":"http","VideoCodec":"vp9","AudioCodec":"opus"}
  ]}}"#;

/// Safari/WebKit: H.264 + HEVC direct-play + native-HLS transcode; no VP9/AV1.
const PROFILE_SAFARI: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264,hevc","AudioCodec":"aac,ac3"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264,hevc","AudioCodec":"aac"}
  ]}}"#;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Surface {
    DirectPlay,
    HlsH264,
    WebmVp9,
}

struct Source {
    id: u64,
    container: &'static str,
    video: &'static str,
    audio: &'static str,
}

async fn seed() -> (web::Data<AppState>, String) {
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
    for s in SOURCES {
        stores
            .put(MediaItem {
                id: s.id,
                path: format!("/m/{}.{}", s.id, s.container).into(),
                title: "V".into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    duration_ms: Some(60_000),
                    width: Some(1920),
                    height: Some(1080),
                    bitrate_bps: Some(4_000_000),
                    container: Some(s.container.into()),
                    video_codec: Some(s.video.into()),
                    audio_codec: Some(s.audio.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

const SOURCES: &[Source] = &[
    Source {
        id: 1,
        container: "mp4",
        video: "h264",
        audio: "aac",
    }, // h264/mp4
    Source {
        id: 2,
        container: "mp4",
        video: "hevc",
        audio: "aac",
    }, // hevc/mp4
    Source {
        id: 3,
        container: "webm",
        video: "vp9",
        audio: "opus",
    }, // vp9/webm
    Source {
        id: 4,
        container: "avi",
        video: "mpeg4",
        audio: "mp3",
    }, // legacy divx
    Source {
        id: 5,
        container: "mp4",
        video: "av1",
        audio: "aac",
    }, // av1/mp4
    Source {
        id: 6,
        container: "webm",
        video: "h264",
        audio: "aac",
    }, // webm/h264 (the trap)
];

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
async fn browser_codec_routing_matrix() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    use Surface::*;
    // (browser label, UA, profile, [(source_id, expected_surface)])
    type Row = (
        &'static str,
        &'static str,
        &'static str,
        &'static [(u64, Surface)],
    );
    let matrix: &[Row] = &[
        (
            "chrome",
            UA_CHROME,
            PROFILE_CHROME,
            &[
                (1, DirectPlay), // h264/mp4 plays natively
                (2, HlsH264),    // hevc → chrome can't; transcode to h264
                (3, DirectPlay), // vp9/webm plays natively
                (4, HlsH264),    // mpeg4/avi → transcode
                (5, DirectPlay), // av1/mp4 plays natively
                (6, HlsH264),    // webm/h264 — no webm/h264 direct rule → transcode
            ],
        ),
        (
            "firefox",
            UA_FIREFOX,
            PROFILE_FIREFOX,
            &[
                (1, WebmVp9),    // h264 UNRELIABLE on firefox → force webm
                (2, WebmVp9),    // hevc → webm
                (3, DirectPlay), // vp9 plays natively → keep direct
                (4, WebmVp9),    // mpeg4 → webm
                (5, DirectPlay), // av1 plays natively → keep direct
                (6, WebmVp9),    // webm/h264 (the trap) → force webm, NOT direct-play
            ],
        ),
        (
            "safari",
            UA_SAFARI,
            PROFILE_SAFARI,
            &[
                (1, DirectPlay), // h264/mp4 native
                (2, DirectPlay), // hevc/mp4 native (Apple)
                (3, HlsH264),    // no vp9 → transcode to h264 (safari plays h264 HLS)
                (4, HlsH264),    // mpeg4 → transcode
                (5, HlsH264),    // no av1 → transcode
                (6, HlsH264),    // webm/h264 no direct rule → transcode
            ],
        ),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (browser, ua, profile, rows) in matrix {
        for (id, expected) in *rows {
            let body = test::call_and_read_body(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/Items/{id}/PlaybackInfo"))
                    .insert_header(("X-Emby-Token", token.as_str()))
                    .insert_header(("User-Agent", *ua))
                    .insert_header(("content-type", "application/json"))
                    .set_payload(profile.to_string())
                    .to_request(),
            )
            .await;
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let ms = &v["MediaSources"][0];
            let direct = ms["SupportsDirectPlay"].as_bool().unwrap_or(false);
            let url = ms["TranscodingUrl"].as_str().unwrap_or("").to_string();
            let proto = ms["TranscodingSubProtocol"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let got = if direct {
                DirectPlay
            } else if url.contains("stream.webm") {
                WebmVp9
            } else if url.contains("master.m3u8") {
                HlsH264
            } else {
                failures.push(format!(
                    "{browser}/src{id}: no recognisable surface (direct={direct} url={url:?})"
                ));
                continue;
            };
            if got != *expected {
                let src = &SOURCES[(*id - 1) as usize];
                failures.push(format!(
                    "{browser} + {}/{}: expected {expected:?}, got {got:?} (direct={direct} proto={proto} url={url})",
                    src.container, src.video
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "routing mismatches:\n  {}",
        failures.join("\n  ")
    );
}
