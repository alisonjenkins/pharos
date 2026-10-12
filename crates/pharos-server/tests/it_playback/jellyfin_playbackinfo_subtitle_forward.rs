#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Task 4 — PlaybackInfo must forward a BURNED text-sub index into the
//! transcode URL, not just image subs.
//!
//! Isolated from the Task 5 DirectPlay-downgrade: the source codec (hevc)
//! doesn't match either client's DirectPlayProfile, so `negotiate()` already
//! returns a Transcode regardless of subtitle handling — these tests prove
//! the URL-forward predicate alone, independent of whether the downgrade
//! exists.

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

// Android/TV-shaped: DirectPlay h264/mp4 only (source is hevc, so this never
// matches — negotiate() always transcodes), and a SubtitleProfiles list that
// covers subrip External but has NO ass entry — the real kotlin SDK shape
// for a client without a libass renderer.
const PROFILE_ANDROID: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
  ],
  "SubtitleProfiles":[
    {"Format":"subrip","Method":"External"}
  ]
}}"#;

// jellyfin-web-shaped: same shape, but SubtitleProfiles declares ass
// External (SubtitlesOctopus/libass renders it) — must NOT be forwarded.
const PROFILE_WEB: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
  ],
  "SubtitleProfiles":[
    {"Format":"ass","Method":"External"},
    {"Format":"subrip","Method":"External"}
  ]
}}"#;

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
            id: 1,
            path: "/m/1.mkv".into(),
            title: "V".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                // hevc matches neither profile's DirectPlayProfile, so
                // negotiate() already returns Transcode — no DirectPlay
                // downgrade is involved in reaching a TranscodingUrl here.
                video_codec: Some("hevc".into()),
                audio_codec: Some("aac".into()),
                // Default-disposition ass track.
                subtitle_tracks: vec![pharos_core::SubtitleTrack {
                    stream_index: 4,
                    codec: Some("ass".into()),
                    language: Some("eng".into()),
                    is_default: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

async fn playback_info(
    state: &web::Data<AppState>,
    token: &str,
    profile_body: &'static str,
) -> serde_json::Value {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token))
            .insert_header(("content-type", "application/json"))
            .set_payload(profile_body)
            .to_request(),
    )
    .await;
    serde_json::from_slice(&raw).unwrap()
}

#[actix_web::test]
async fn android_without_external_ass_forwards_the_burned_index() {
    let (state, token) = seed().await;
    let v = playback_info(&state, &token, PROFILE_ANDROID).await;
    let source = &v["MediaSources"][0];
    // Already transcoding regardless of subtitle handling (hevc source).
    let url = source["TranscodingUrl"]
        .as_str()
        .expect("hevc source must always transcode for this client");
    assert!(
        url.contains("SubtitleStreamIndex=4"),
        "a burned (no-external-profile) ass default must ride the transcode URL: {url}"
    );
}

#[actix_web::test]
async fn web_with_external_ass_does_not_forward_the_index() {
    let (state, token) = seed().await;
    let v = playback_info(&state, &token, PROFILE_WEB).await;
    let source = &v["MediaSources"][0];
    let url = source["TranscodingUrl"]
        .as_str()
        .expect("hevc source must always transcode for this client");
    assert!(
        !url.contains("SubtitleStreamIndex=4"),
        "an externally-rendered ass default must NOT ride the transcode URL: {url}"
    );
}
