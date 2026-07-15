#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B73 — native-player direct-play auth workaround.
//!
//! A remote NON-web client (Jellyfin Android TV / mobile / Kodi) that would
//! DirectPlay streams the file from `/videos/{id}/stream?static=true`, but its
//! ExoPlayer/okhttp data source has no auth interceptor and builds that URL
//! with NO token → 401 "missing token" → playback dies the instant it starts
//! (confirmed live via the B72 header audit: authorization / x-emby-token /
//! api_key all absent). pharos steers every non-web VIDEO direct-play onto the
//! authed HLS transcode surface, whose TranscodingUrl embeds api_key, and drops
//! SupportsDirectPlay/Stream to false so the client can't retry the tokenless
//! direct URL. jellyfin-web keeps true direct-play (its `<video src>` carries
//! api_key + it gets the JellyfinAuth cookie).

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

// A profile that DIRECT-PLAYS the h264/mp4/aac source below (container + both
// codecs match a DirectPlayProfile) → the negotiator returns DirectPlay.
const PROFILE_DIRECTPLAY: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
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
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

// The browser-vs-native split is the User-Agent: jellyfin-web is `Mozilla/…`;
// the Android TV app's media stack is `Jellyfin Android TV/… (OkHttp/…)`.
const UA_ANDROID_TV: &str = "Jellyfin Android TV/0.19.9 via jellyfin-sdk-kotlin (OkHttp/4.12.0)";
const UA_BROWSER: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0";

async fn playbackinfo(user_agent: &str) -> serde_json::Value {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_DIRECTPLAY).unwrap()["DeviceProfile"],
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", user_agent))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    serde_json::from_slice(&raw).unwrap()
}

#[actix_web::test]
async fn native_client_directplay_is_steered_to_authed_hls() {
    // Android TV: the tokenless-direct-URL path is dead, so pharos must NOT
    // advertise direct play — it hands the authed HLS TranscodingUrl instead.
    let v = playbackinfo(UA_ANDROID_TV).await;
    let ms = &v["MediaSources"][0];
    assert_eq!(
        ms["SupportsDirectPlay"].as_bool(),
        Some(false),
        "native client must not be offered direct play (its ExoPlayer builds a \
         tokenless /stream?static=true → 401)"
    );
    assert_eq!(
        ms["SupportsDirectStream"].as_bool(),
        Some(false),
        "and not direct stream either, or it retries the same tokenless URL"
    );
    let url = ms["TranscodingUrl"].as_str().unwrap_or_default();
    assert!(
        url.contains("/master.m3u8"),
        "must fall onto the authed H.264 HLS surface, got {url:?}"
    );
    assert!(
        url.contains("api_key="),
        "the TranscodingUrl MUST carry the token — that is the whole point: {url:?}"
    );
}

#[actix_web::test]
async fn web_client_keeps_true_direct_play() {
    // jellyfin-web self-authenticates the direct URL (api_key in `<video src>`
    // + JellyfinAuth cookie), so it must keep byte-served direct play — never
    // forced onto a needless re-encode.
    let v = playbackinfo(UA_BROWSER).await;
    let ms = &v["MediaSources"][0];
    assert_eq!(
        ms["SupportsDirectPlay"].as_bool(),
        Some(true),
        "web client must keep direct play"
    );
    assert!(
        ms["TranscodingUrl"].is_null(),
        "a direct-playing web client gets no TranscodingUrl: {:?}",
        ms["TranscodingUrl"]
    );
}
