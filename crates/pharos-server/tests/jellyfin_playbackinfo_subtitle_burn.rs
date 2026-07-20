#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Task 3 — PlaybackInfo must resolve each embedded TEXT subtitle track's
//! delivery per the client's SubtitleProfiles (Task 2's
//! `decide_subtitle_delivery`), not unconditionally advertise External.
//!
//! Android/TV (kotlin SDK) never declares an External SubtitleProfile for
//! `ass` — it has no libass renderer. Advertising `DeliveryMethod:"External"`
//! for an ass track there makes the client either skip the subtitle or try
//! to render the raw ASS text itself (black bars); it must see `"Encode"`
//! with no DeliveryUrl so it requests the burn transcode instead.
//! jellyfin-web (SubtitlesOctopus) DOES declare ass External and must keep
//! getting the External raw-.ass URL unchanged.

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
                subtitle_tracks: vec![pharos_core::SubtitleTrack {
                    stream_index: 4,
                    codec: Some("ass".into()),
                    language: Some("eng".into()),
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

fn find_ass_stream(v: &serde_json::Value) -> serde_json::Value {
    v["MediaSources"][0]["MediaStreams"]
        .as_array()
        .expect("MediaStreams")
        .iter()
        .find(|s| s["Index"] == 4)
        .expect("ass stream at index 4")
        .clone()
}

#[actix_web::test]
async fn android_profile_without_external_ass_burns_the_ass_track() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_ANDROID)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ass = find_ass_stream(&v);
    assert_eq!(
        ass["DeliveryMethod"], "Encode",
        "Android has no ass SubtitleProfile → must burn, got {ass}"
    );
    assert_eq!(ass["SupportsExternalStream"], false, "{ass}");
    assert!(
        ass.get("DeliveryUrl").is_none() || ass["DeliveryUrl"].is_null(),
        "a burned track must carry no DeliveryUrl: {ass}"
    );
}

#[actix_web::test]
async fn web_profile_with_external_ass_stays_external() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_WEB)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ass = find_ass_stream(&v);
    assert_eq!(
        ass["DeliveryMethod"], "External",
        "jellyfin-web declares ass External → must stay External, got {ass}"
    );
    assert_eq!(ass["SupportsExternalStream"], true, "{ass}");
    assert!(
        ass["DeliveryUrl"]
            .as_str()
            .unwrap_or_default()
            .ends_with("Stream.ass"),
        "{ass}"
    );
}
