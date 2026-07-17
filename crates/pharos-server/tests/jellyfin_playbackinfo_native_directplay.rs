#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B75 — native-player direct-play via a capability token.
//!
//! A remote NON-web client (Jellyfin Android TV / mobile / Kodi) that
//! DirectPlays streams the file from `/videos/{id}/stream?static=true`, but its
//! ExoPlayer/okhttp data source has no auth interceptor and builds that URL
//! with NO token (confirmed live via the B72 header audit + reading the SDK).
//! The old B73 workaround force-transcoded every non-web client to dodge the
//! 401 — wrong for every device that can decode the source, and on
//! memory-tight TVs the needless transcode + HLS churn on seek got the app
//! OOM-killed.
//!
//! B75 instead authenticates the native stream: the MediaSource `ETag` equals
//! the PlaySessionId (a random uuid registered against this media id), and the
//! Jellyfin SDK forwards it verbatim as `?tag=` on the `/stream` URL. So the
//! native client KEEPS true direct play; the stream route authorizes the
//! tokenless request by the tag→session→media_id binding. jellyfin-web is
//! unchanged (its `<video src>` carries api_key + the JellyfinAuth cookie).

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
    // B86 — an Audio track for the music direct-play auth test.
    stores
        .put(MediaItem {
            id: 2,
            path: "/m/2.mp3".into(),
            title: "Track".into(),
            kind: MediaKind::Audio,
            probe: MediaProbe {
                duration_ms: Some(180_000),
                bitrate_bps: Some(256_000),
                container: Some("mp3".into()),
                audio_codec: Some("mp3".into()),
                audio_channels: Some(2),
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

macro_rules! init_app {
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

#[actix_web::test]
async fn native_client_keeps_direct_play_with_capability_etag() {
    // Android TV: pharos now trusts the negotiator (DirectPlay) AND stamps a
    // capability token into ETag so the tokenless native /stream authenticates.
    let (state, token) = seed().await;
    let app = init_app!(state);
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_DIRECTPLAY).unwrap()["DeviceProfile"],
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_ANDROID_TV))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ms = &v["MediaSources"][0];
    assert_eq!(
        ms["SupportsDirectPlay"].as_bool(),
        Some(true),
        "native client keeps direct play (no needless transcode): {ms}"
    );
    assert!(
        ms["TranscodingUrl"].is_null(),
        "direct-playing native client gets no TranscodingUrl: {:?}",
        ms["TranscodingUrl"]
    );
    // The capability token: ETag == PlaySessionId, non-empty, and the SDK
    // forwards it as ?tag= on the /stream URL.
    let etag = ms["ETag"].as_str().unwrap_or_default();
    assert!(!etag.is_empty(), "ETag capability token must be set: {ms}");
    assert_eq!(
        Some(etag),
        v["PlaySessionId"].as_str(),
        "ETag must equal PlaySessionId so the registered session authorizes the stream"
    );
}

#[actix_web::test]
async fn native_capability_etag_authorizes_tokenless_stream() {
    // End-to-end: PlaybackInfo (authed) → grab ETag → GET /stream?tag=<etag>
    // with NO token (exactly the native ExoPlayer request) must NOT 401.
    let (state, token) = seed().await;
    let app = init_app!(state);
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_DIRECTPLAY).unwrap()["DeviceProfile"],
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_ANDROID_TV))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let etag = v["MediaSources"][0]["ETag"].as_str().unwrap().to_string();

    // No token, just the tag — the native ExoPlayer shape. The file doesn't
    // exist on disk (seed path is fake) so a 404 is fine; the point is it must
    // pass AUTH — never 401 "missing token".
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/1/stream?static=true&tag={etag}"))
            .insert_header(("User-Agent", "okhttp/4.12.0"))
            .to_request(),
    )
    .await;
    assert_ne!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "a valid capability tag must authorize the tokenless native stream"
    );
}

#[actix_web::test]
async fn native_capability_etag_authorizes_tokenless_audio_stream() {
    // B86 — music DirectPlay: the Android TV app fetches
    // /Audio/{id}/stream?static=true&tag=<ETag> with NO token (ExoPlayer's raw
    // fetch). stream_audio used the strict AuthUser extractor and 401'd, so no
    // music played. It must authorize via the ETag capability like the video
    // route (B75). File is fake on disk (404 ok); the point is it must NOT 401.
    let (state, token) = seed().await;
    let app = init_app!(state);
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_DIRECTPLAY).unwrap()["DeviceProfile"],
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/2/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_ANDROID_TV))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let etag = v["MediaSources"][0]["ETag"].as_str().unwrap().to_string();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/audio/2/stream?static=true&tag={etag}"))
            .insert_header(("User-Agent", "okhttp/4.12.0"))
            .to_request(),
    )
    .await;
    assert_ne!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "a valid capability tag must authorize the tokenless native audio stream"
    );
}

#[actix_web::test]
async fn wrong_tag_does_not_authorize_stream() {
    // A tag that resolves to no session (or a different item) must NOT grant
    // access — the capability is item-scoped and unguessable.
    let (state, _token) = seed().await;
    let app = init_app!(state);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/videos/1/stream?static=true&tag=deadbeefdeadbeefdeadbeefdeadbeef")
            .insert_header(("User-Agent", "okhttp/4.12.0"))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "an unregistered tag must be rejected"
    );
}

#[actix_web::test]
async fn web_client_keeps_true_direct_play() {
    // jellyfin-web self-authenticates the direct URL (api_key in `<video src>`
    // + JellyfinAuth cookie), so it keeps byte-served direct play with no
    // TranscodingUrl.
    let (state, token) = seed().await;
    let app = init_app!(state);
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_DIRECTPLAY).unwrap()["DeviceProfile"],
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_BROWSER))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
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
