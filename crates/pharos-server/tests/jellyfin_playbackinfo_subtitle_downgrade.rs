#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tasks 4 & 5 — PlaybackInfo forwards a burned text-sub index into the
//! transcode URL, and downgrades a DirectPlay-eligible source to Transcode
//! when the SELECTED subtitle must burn (no video stream to burn into
//! otherwise).
//!
//! Android/TV (kotlin SDK) declares no `ass` SubtitleProfile at all (no
//! libass renderer) — its default-disposition `ass` track must burn, which
//! means an otherwise-DirectPlay-eligible h264/mp4 source must downgrade to
//! a TranscodingUrl carrying `SubtitleStreamIndex=<idx>`, and
//! `SupportsDirectStream` must resolve `false`. jellyfin-web (SubtitlesOctopus)
//! declares `ass` External and must keep DirectPlay untouched.

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

// Android/TV-shaped: DirectPlay h264/mp4, and a SubtitleProfiles list that
// covers subrip External but has NO ass entry at all — the real kotlin SDK
// shape for a client without a libass renderer.
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

// jellyfin-web-shaped: same DirectPlay/Transcoding, but SubtitleProfiles
// explicitly declares ass External (SubtitlesOctopus/libass renders it).
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
            path: "/m/1.mp4".into(),
            title: "V".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                // Default-disposition ass track — no client pick in the
                // request, so `resolve_selected_subtitle` falls back to it.
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
async fn android_profile_default_ass_downgrades_to_transcode_and_forwards_index() {
    let (state, token) = seed().await;
    let v = playback_info(&state, &token, PROFILE_ANDROID).await;
    let source = &v["MediaSources"][0];

    // Task 5: no external ass rendition exists for this client, so the
    // otherwise-DirectPlay-eligible source must downgrade to Transcode.
    assert_eq!(
        source["SupportsDirectStream"], false,
        "android client selecting a burned ass default must not get DirectStream: {source}"
    );
    let url = source["TranscodingUrl"]
        .as_str()
        .expect("a TranscodingUrl must be emitted so the burn has a video stream");

    // Task 4: the burn index must ride the transcode URL so the segment
    // handler actually burns it in.
    assert!(
        url.contains("SubtitleStreamIndex=4"),
        "burned ass index must be forwarded into the TranscodingUrl: {url}"
    );
}

#[actix_web::test]
async fn web_profile_with_external_ass_keeps_directplay_no_forward() {
    let (state, token) = seed().await;
    let v = playback_info(&state, &token, PROFILE_WEB).await;
    let source = &v["MediaSources"][0];

    // Task 5: ass renders externally for this client — no downgrade needed.
    assert_eq!(
        source["SupportsDirectStream"], true,
        "web client with external ass support must keep DirectStream: {source}"
    );
    // No transcode URL at all for a pure DirectPlay verdict.
    assert!(
        source.get("TranscodingUrl").is_none() || source["TranscodingUrl"].is_null(),
        "{source}"
    );
}

#[actix_web::test]
async fn android_profile_explicit_off_pick_does_not_downgrade() {
    let (state, token) = seed().await;
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
            .uri("/Items/1/PlaybackInfo?SubtitleStreamIndex=-1")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_ANDROID)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let source = &v["MediaSources"][0];
    // Subtitle explicitly turned off → nothing to burn → stays DirectPlay.
    assert_eq!(
        source["SupportsDirectStream"], true,
        "explicit SubtitleStreamIndex=-1 must not force a transcode: {source}"
    );
}
