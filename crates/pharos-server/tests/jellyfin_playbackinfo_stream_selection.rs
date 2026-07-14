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
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

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
                // Stream 5 = image sub (PGS → must burn), stream 6 = text sub
                // (subrip → delivered External, must NOT be baked into the URL).
                subtitle_tracks: vec![
                    pharos_core::SubtitleTrack {
                        stream_index: 5,
                        codec: Some("hdmv_pgs_subtitle".into()),
                        // Avatar-shaped: the forced-Na'vi track ships
                        // default+forced so it plays with no user action.
                        is_default: true,
                        is_forced: true,
                        ..Default::default()
                    },
                    pharos_core::SubtitleTrack {
                        stream_index: 6,
                        codec: Some("subrip".into()),
                        ..Default::default()
                    },
                ],
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
        "an IMAGE sub (PGS, stream 5) MUST burn → its index rides the URL: {url:?}"
    );
}

#[actix_web::test]
async fn text_subtitle_index_is_not_baked_into_transcoding_url() {
    // Regression: a TEXT/ASS sub is delivered as a separate External rendition
    // the client renders — baking its index into the transcode URL makes the
    // VP9 segments burn it in, which (via output-seek decode-from-0) takes tens
    // of seconds per segment deep in a file and stutters playback. Stream 6 is
    // subrip → the URL must NOT carry SubtitleStreamIndex.
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_FIREFOX).unwrap()["DeviceProfile"],
        "SubtitleStreamIndex": 6,
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
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.contains("/vp9/master.m3u8"),
        "expected the Firefox VP9 transcode surface, got {url:?}"
    );
    assert!(
        !url.contains("SubtitleStreamIndex"),
        "a text sub must be delivered External, not burned — index must be \
         withheld from the transcode URL: {url:?}"
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

// B41 — the H.264 HLS surface (non-VP9 clients) must carry stream selection
// exactly like the VP9 one: the h264 master URL used to drop it, so a
// selected PGS subtitle never reached the segment handler (burn silently
// off) and an explicit audio track fell back to the default.
const PROFILE_H264: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
  ]
}}"#;

#[actix_web::test]
async fn h264_master_url_carries_stream_selection() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_H264).unwrap()["DeviceProfile"],
        "AudioStreamIndex": 3,
        "SubtitleStreamIndex": 5,
    });
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.to_string())
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.contains("/master.m3u8") && !url.contains("/vp9/"),
        "expected the H.264 HLS surface, got {url:?}"
    );
    assert!(
        url.contains("AudioStreamIndex=3"),
        "h264 TranscodingUrl must carry the audio pick (B41): {url:?}"
    );
    assert!(
        url.contains("SubtitleStreamIndex=5"),
        "h264 TranscodingUrl must carry the image-sub burn index (B41): {url:?}"
    );
}

// B43 — the Firefox VP9 force is a LINUX-desktop quirk (distro builds can
// lack the system H.264 decoder while canPlayType lies). Firefox on macOS /
// Windows / Android decodes H.264 natively; forcing them onto the VP9 encode
// path turned an h264 source's near-instant remux into a ~2.5s-per-segment
// libvpx encode — the dominant cost of every seek.
const UA_FIREFOX_MAC: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:152.0) Gecko/20100101 Firefox/152.0";

/// What jellyfin-web actually advertises on an H.264-capable Firefox
/// (macOS/Windows): h264 direct-play + h264 HLS transcode ahead of the VP9
/// fallback. The Linux force must override this order; Mac must not.
const PROFILE_FIREFOX_BOTH: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[
    {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"},
    {"Container":"webm","Type":"Video","VideoCodec":"vp8,vp9,av1","AudioCodec":"opus,vorbis"}
  ],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"},
    {"Container":"webm","Type":"Video","Protocol":"http","VideoCodec":"vp9","AudioCodec":"opus"}
  ]
}}"#;

#[actix_web::test]
async fn mac_firefox_is_not_forced_onto_vp9() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    // Realistic dual profile — only the UA differs from the Linux case.
    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("User-Agent", UA_FIREFOX_MAC))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_FIREFOX_BOTH)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ms = &v["MediaSources"][0];
    let url = ms["TranscodingUrl"].as_str().unwrap_or_default();
    assert!(
        !url.contains("/vp9/"),
        "Mac Firefox decodes H.264 natively — must not be forced onto VP9: {url:?}"
    );
    // h264-in-mp4 source + h264 direct-play profile → direct play, the
    // fastest possible path (no transcode at all).
    assert_eq!(
        ms["SupportsDirectPlay"], true,
        "h264/mp4 source should direct-play on an h264-capable client: {ms}"
    );
}

#[actix_web::test]
async fn linux_firefox_still_forced_onto_vp9() {
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
            .insert_header(("User-Agent", UA_FIREFOX))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_FIREFOX_BOTH)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.contains("/vp9/master.m3u8"),
        "desktop-Linux Firefox keeps the VP9 force (its canPlayType lies): {url:?}"
    );
}

#[actix_web::test]
async fn default_image_sub_is_baked_without_client_pick() {
    // B44 — Avatar's forced-Na'vi PGS track is default+forced: with NO
    // client subtitle pick the transcode must burn it (the client is told
    // it's active via DefaultSubtitleStreamIndex; not baking it silently
    // played Na'vi sections unsubtitled).
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
            .insert_header(("User-Agent", UA_FIREFOX))
            .insert_header(("content-type", "application/json"))
            .set_payload(PROFILE_FIREFOX)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ms = &v["MediaSources"][0];
    let url = ms["TranscodingUrl"].as_str().unwrap_or_default();
    assert!(
        url.contains("SubtitleStreamIndex=5"),
        "default image sub must be baked into the transcode URL: {url:?}"
    );
    assert_eq!(ms["DefaultSubtitleStreamIndex"], 5, "{ms}");
}

#[actix_web::test]
async fn explicit_subtitles_off_beats_the_default_track() {
    // A client that turned subtitles OFF (-1) must not get the default
    // image track burned back in.
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let body = serde_json::json!({
        "DeviceProfile": serde_json::from_str::<serde_json::Value>(PROFILE_FIREFOX).unwrap()["DeviceProfile"],
        "SubtitleStreamIndex": -1,
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
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.contains("SubtitleStreamIndex=-1"),
        "explicit off must ride through: {url:?}"
    );
    assert!(
        !url.contains("SubtitleStreamIndex=5"),
        "explicit off must beat the default track: {url:?}"
    );
}
