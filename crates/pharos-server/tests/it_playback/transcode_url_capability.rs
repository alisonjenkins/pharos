//! V1050 / B1090 — no transcode, subtitle or font URL carries the caller's
//! token. `PlaybackInfo` hands out URLs whose only credential is the
//! PlaySessionId, and the HLS, universal-audio and subtitle routes accept a
//! live one for its own item (017 T064).

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SubtitleTrack, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// No direct play, so every source gets a TranscodingUrl.
const PROFILE_TRANSCODE: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[],
  "TranscodingProfiles":[
    {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"},
    {"Container":"aac","Type":"Audio","Protocol":"http","AudioCodec":"aac"}
  ],
  "SubtitleProfiles":[{"Format":"vtt","Method":"External"}]
}}"#;

const UA_ANDROID_TV: &str = "Jellyfin Android TV/0.19.9 via jellyfin-sdk-kotlin (OkHttp/4.12.0)";

async fn seed() -> (web::Data<AppState>, String, UserId) {
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
    for (id, kind, probe) in [
        (
            1,
            MediaKind::Movie,
            MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mkv".into()),
                video_codec: Some("hevc".into()),
                audio_codec: Some("ac3".into()),
                subtitle_tracks: vec![SubtitleTrack {
                    stream_index: 2,
                    codec: Some("subrip".into()),
                    language: Some("eng".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ),
        (
            2,
            MediaKind::Audio,
            MediaProbe {
                duration_ms: Some(180_000),
                bitrate_bps: Some(1_000_000),
                container: Some("flac".into()),
                audio_codec: Some("flac".into()),
                audio_channels: Some(2),
                ..Default::default()
            },
        ),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.bin").into(),
                title: "V".into(),
                kind,
                probe,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string(), uid)
}

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

macro_rules! playback_info {
    ($app:expr, $token:expr, $id:expr) => {{
        let raw = test::call_and_read_body(
            $app,
            test::TestRequest::post()
                .uri(&format!("/Items/{}/PlaybackInfo", $id))
                .insert_header(("X-Emby-Token", $token))
                .insert_header(("User-Agent", UA_ANDROID_TV))
                .insert_header(("content-type", "application/json"))
                .set_payload(PROFILE_TRANSCODE)
                .to_request(),
        )
        .await;
        serde_json::from_slice::<serde_json::Value>(&raw).unwrap()
    }};
}

/// A GET with no credential but what the URL carries: ExoPlayer's shape.
macro_rules! bare_get {
    ($app:expr, $uri:expr) => {{
        let resp = test::call_service(
            $app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("User-Agent", "okhttp/4.12.0"))
                .to_request(),
        )
        .await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        (status, String::from_utf8_lossy(&body).into_owned())
    }};
}

fn transcoding_url(v: &serde_json::Value) -> String {
    v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a TranscodingUrl: {v}"))
        .to_string()
}

#[actix_web::test]
async fn transcoding_urls_carry_the_play_session_not_the_token() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    for id in [1, 2] {
        let v = playback_info!(&app, token.as_str(), id);
        let url = transcoding_url(&v);
        let psid = v["PlaySessionId"].as_str().unwrap();
        assert!(!url.contains(&token), "token leaked into {url}");
        assert!(!url.to_ascii_lowercase().contains("api_key"), "{url}");
        assert!(url.contains(&format!("PlaySessionId={psid}")), "{url}");
    }
}

#[actix_web::test]
async fn subtitle_delivery_urls_carry_the_play_session_not_the_token() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let v = playback_info!(&app, token.as_str(), 1);
    let psid = v["PlaySessionId"].as_str().unwrap();
    let urls: Vec<&str> = v["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["DeliveryUrl"].as_str())
        .collect();
    assert!(!urls.is_empty(), "expected a subtitle DeliveryUrl: {v}");
    for url in urls {
        assert!(!url.contains(&token), "token leaked into {url}");
        assert!(url.contains(&format!("PlaySessionId={psid}")), "{url}");
        let (status, _) = bare_get!(&app, url);
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{url}");
    }
}

#[actix_web::test]
async fn a_tokenless_master_and_its_variant_play_on_the_play_session() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 1));
    let (status, master) = bare_get!(&app, &url);
    assert_eq!(status, StatusCode::OK, "{master}");
    assert!(!master.contains(&token), "{master}");
    assert!(!master.contains("api_key"), "{master}");
    let variant = master
        .lines()
        .find(|l| !l.starts_with('#') && l.contains(".m3u8"))
        .unwrap_or_else(|| panic!("no variant in {master}"));
    assert!(variant.contains("PlaySessionId="), "{variant}");
    let (status, body) = bare_get!(&app, variant);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("api_key"), "{body}");
}

#[actix_web::test]
async fn a_tokenless_universal_audio_url_passes_auth() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 2));
    assert!(
        url.starts_with("/audio/") && url.contains("/universal?"),
        "{url}"
    );
    // The file is not on disk; anything but a 401 means auth passed.
    let (status, body) = bare_get!(&app, &url);
    assert_ne!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[actix_web::test]
async fn an_unknown_or_foreign_play_session_is_refused() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let v = playback_info!(&app, token.as_str(), 1);
    let psid = v["PlaySessionId"].as_str().unwrap();
    for uri in [
        "/videos/1/master.m3u8?PlaySessionId=deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        "/videos/1/master.m3u8".to_string(),
        // A live session for item 1 opens nothing of item 2.
        format!("/audio/2/universal?PlaySessionId={psid}"),
        format!("/videos/2/master.m3u8?PlaySessionId={psid}"),
        format!("/videos/2/0/subtitles/2/stream.vtt?PlaySessionId={psid}"),
    ] {
        let (status, body) = bare_get!(&app, &uri);
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
    }
}

#[actix_web::test]
async fn a_revoked_holder_ends_the_capability() {
    let (state, token, _) = seed().await;
    let app = init_app!(state.clone());
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 1));
    state.stores.revoke(&token).await.unwrap();
    let (status, body) = bare_get!(&app, &url);
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[actix_web::test]
async fn a_header_authed_master_with_its_play_session_embeds_no_token() {
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 1));
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let master = String::from_utf8_lossy(&body);
    assert!(master.contains("#EXTM3U"), "{master}");
    assert!(!master.contains(&token), "{master}");
}

#[actix_web::test]
async fn a_master_without_a_live_play_session_still_hands_its_token_on() {
    // A client that builds its own master URL and names no registered
    // session keeps working: its segments still need the token it sent.
    let (state, token, _) = seed().await;
    let app = init_app!(state);
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/1/master.m3u8?api_key={token}"))
            .to_request(),
    )
    .await;
    let master = String::from_utf8_lossy(&body);
    assert!(master.contains(&format!("api_key={token}")), "{master}");
}

#[actix_web::test]
async fn each_credential_is_counted() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let rec = DebuggingRecorder::new();
    let _g = metrics::set_default_local_recorder(&rec);
    let (state, token, _) = seed().await;
    let app = init_app!(state.clone());
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 1));
    let _ = bare_get!(&app, &url);
    let _ = bare_get!(&app, "/videos/1/master.m3u8?PlaySessionId=nope");
    let _ = bare_get!(&app, &format!("/videos/1/master.m3u8?api_key={token}"));
    let psid = url
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("PlaySessionId="))
        .unwrap()
        .to_string();
    let _ = bare_get!(&app, &format!("/videos/2/master.m3u8?PlaySessionId={psid}"));
    state.stores.revoke(&token).await.unwrap();
    let _ = bare_get!(&app, &url);
    let rows = pharos_test_metrics::snapshotter(&rec).snapshot().into_vec();
    let count = |name: &str, key: &str, value: &str| {
        rows.iter()
            .find_map(|(ck, _, _, v)| {
                let hit = ck.key().name() == name
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == key && l.value() == value);
                match v {
                    DebugValue::Counter(n) if hit => Some(*n),
                    _ => None,
                }
            })
            .unwrap_or(0)
    };
    assert_eq!(
        count(
            "pharos_playback_credential_total",
            "credential",
            "capability"
        ),
        1
    );
    assert_eq!(
        count("pharos_playback_credential_total", "credential", "query"),
        1
    );
    assert_eq!(
        count("pharos_play_capability_total", "outcome", "granted"),
        1
    );
    assert_eq!(
        count("pharos_play_capability_total", "outcome", "no_session"),
        1
    );
    assert_eq!(
        count("pharos_play_capability_total", "outcome", "wrong_item"),
        1
    );
    assert_eq!(
        count("pharos_play_capability_total", "outcome", "holder_ended"),
        1
    );
}

#[actix_web::test]
async fn a_capability_answers_to_the_holders_current_policy() {
    // The capability names the user; what they may see is read on each use,
    // so a library taken away after PlaybackInfo hides the item (V187), with
    // no token and with a stale cookie alike.
    let (state, token, uid) = seed().await;
    // A real sidecar, so the subtitle route serves 200 until the policy moves.
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("subbed.mkv");
    std::fs::write(&video, b"x").unwrap();
    std::fs::write(
        dir.path().join("subbed.eng.vtt"),
        b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n",
    )
    .unwrap();
    state
        .stores
        .put(MediaItem {
            id: 3,
            path: video,
            title: "Subbed".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                container: Some("mkv".into()),
                video_codec: Some("hevc".into()),
                audio_codec: Some("ac3".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let app = init_app!(state.clone());
    let v = playback_info!(&app, token.as_str(), 3);
    let master = transcoding_url(&v);
    let sub = v["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|s| s["DeliveryUrl"].as_str())
        .unwrap_or_else(|| panic!("expected the sidecar's DeliveryUrl: {v}"))
        .to_string();
    let cases = [
        (&master, false),
        (&master, true),
        (&sub, false),
        (&sub, true),
    ];
    let get = |uri: &str, stale_cookie: bool| {
        let mut req = test::TestRequest::get().uri(uri);
        if stale_cookie {
            req = req.insert_header(("Cookie", "JellyfinAuth=0123456789abcdef0123456789abcdef"));
        }
        req.to_request()
    };
    for (uri, cookie) in cases {
        let status = test::call_service(&app, get(uri, cookie)).await.status();
        assert!(
            status == StatusCode::OK,
            "{uri} stale_cookie={cookie}: {status}"
        );
    }
    state
        .stores
        .set_policy(
            uid,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    for (uri, cookie) in cases {
        let status = test::call_service(&app, get(uri, cookie)).await.status();
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} stale_cookie={cookie}");
    }
}

#[actix_web::test]
async fn a_playlist_on_another_tokens_session_keeps_its_own_token() {
    // The user signs in again after the token that negotiated the session was
    // revoked: the dead session must not strip the new token from the
    // playlist's URLs, or every child request would 401.
    let (state, token, uid) = seed().await;
    let app = init_app!(state.clone());
    let url = transcoding_url(&playback_info!(&app, token.as_str(), 1));
    let fresh = state.stores.issue(uid, "t2").await.unwrap();
    let fresh = fresh.0.expose().to_string();
    state.stores.revoke(&token).await.unwrap();
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("{url}&api_key={fresh}"))
            .to_request(),
    )
    .await;
    let master = String::from_utf8_lossy(&body);
    assert!(master.contains(&format!("api_key={fresh}")), "{master}");
}

#[actix_web::test]
async fn a_playlist_whose_session_is_not_stored_keeps_its_token() {
    // The durable write can fail while this replica's cache still holds the
    // session; child requests check the stored row, so binding on the cache
    // alone would strip the only credential that still works (B1091).
    use pharos_core::TranscodeSessionStore as _;
    let (state, token, _) = seed().await;
    let app = init_app!(state.clone());
    let v = playback_info!(&app, token.as_str(), 1);
    let psid = v["PlaySessionId"].as_str().unwrap().to_string();
    let url = transcoding_url(&v);
    state.stores.remove_transcode_session(&psid).await.unwrap();
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("{url}&api_key={token}"))
            .to_request(),
    )
    .await;
    let master = String::from_utf8_lossy(&body);
    assert!(master.contains(&format!("api_key={token}")), "{master}");
}
