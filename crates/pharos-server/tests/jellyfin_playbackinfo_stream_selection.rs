#![allow(clippy::unwrap_used, clippy::expect_used)]
//! PlaybackInfo must honour `AudioStreamIndex` / `SubtitleStreamIndex` sent in
//! the POST **body**, not only the query string.
//!
//! When the user switches audio track on a TRANSCODED HLS stream, jellyfin-web
//! (10.11.x) does a full stream reload: it re-POSTs `/Items/{id}/PlaybackInfo`
//! with the new `AudioStreamIndex` inside the `playbackInfoDto` **body**
//! (`c.AudioStreamIndex = …; getPostedPlaybackInfo({playbackInfoDto:c})`),
//! reads the fresh `MediaSources[0].TranscodingUrl`, tears down the old
//! encoding, and plays the new URL. If the server only reads the index from
//! the query string, the returned TranscodingUrl carries the DEFAULT audio and
//! the switch silently does nothing — the reported "audio switching broken on
//! Firefox/VP9" bug.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

// Firefox/Zen: forces the VP9-in-fMP4 HLS transcode surface (no reliable H.264
// in MSE), so the TranscodingUrl is the `/vp9/master.m3u8` we thread selection
// into.
const UA_FIREFOX: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0";
const PROFILE_FIREFOX: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"webm","Type":"Video","VideoCodec":"vp8,vp9,av1","AudioCodec":"opus,vorbis"}
  ],
  "TranscodingProfiles":[
    {"Container":"webm","Type":"Video","Protocol":"http","VideoCodec":"vp9","AudioCodec":"opus"}
  ]
}}"#;

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
    // H.264/mp4 source → Firefox can't rely on it → transcodes to VP9.
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
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
async fn audio_stream_index_in_body_reaches_transcoding_url() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // jellyfin-web's audio-switch reload: AudioStreamIndex lives in the body
    // alongside the DeviceProfile.
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_FIREFOX).unwrap()["DeviceProfile"],
        "AudioStreamIndex": 3,
        "SubtitleStreamIndex": 5,
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_FIREFOX))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ms = &v["MediaSources"][0];

    let url = ms["TranscodingUrl"].as_str().unwrap_or_default();
    assert!(
        url.contains("/vp9/master.m3u8"),
        "expected the Firefox VP9 transcode surface, got {url:?}"
    );
    assert!(
        url.contains("AudioStreamIndex=3"),
        "TranscodingUrl must carry the body's AudioStreamIndex so the reloaded \
         stream uses the chosen track: {url:?}"
    );
    assert!(
        url.contains("SubtitleStreamIndex=5"),
        "TranscodingUrl must carry the body's SubtitleStreamIndex: {url:?}"
    );
}

#[actix_web::test]
async fn query_string_stream_selection_still_honoured() {
    // Legacy / other clients pass the indices on the query string — must keep
    // working (and the query wins if somehow both are present).
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
            .uri("/Items/1/PlaybackInfo?AudioStreamIndex=2")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_FIREFOX))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_FIREFOX.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.contains("AudioStreamIndex=2"),
        "query-string AudioStreamIndex must still thread into the URL: {url:?}"
    );
}
