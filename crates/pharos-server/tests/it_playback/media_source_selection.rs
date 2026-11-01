//! A client that picks a grouped edition (P34) echoes its MediaSource Id back
//! as `MediaSourceId`; every playback route must then read the edition's own
//! file and probe, not the item's.
//!
//! Item 7: primary `Film.mp4` (h264/aac, direct plays, 60 s) with one edition
//! `Film - 3D.HSBS.avi` (mpeg4/mp3, must transcode, 30 s).

use actix_web::{test, web, App};
use pharos_core::{
    AlternateMediaSource, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore, Video3DFormat,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;

const PROFILE: &str = r#""DeviceProfile":{
  "DirectPlayProfiles":[{"Type":"Video","Container":"mp4","VideoCodec":"h264","AudioCodec":"aac"}],
  "TranscodingProfiles":[{"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}]
}"#;

const PRIMARY_BYTES: &[u8] = b"primary file bytes";
const EDITION_BYTES: &[u8] = b"the 3D edition's own bytes";

fn wire() -> String {
    pharos_jellyfin_api::dto::wire_item_id(7)
}

fn edition_id() -> String {
    format!("{}-3d-hsbs", wire())
}

async fn setup(td: &std::path::Path) -> (web::Data<AppState>, String) {
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

    let primary = td.join("Film.mp4");
    let edition = td.join("Film - 3D.HSBS.avi");
    tokio::fs::write(&primary, PRIMARY_BYTES).await.unwrap();
    tokio::fs::write(&edition, EDITION_BYTES).await.unwrap();
    let edition_probe = MediaProbe {
        duration_ms: Some(30_000),
        width: Some(1920),
        height: Some(1080),
        bitrate_bps: Some(1_000_000),
        size_bytes: Some(EDITION_BYTES.len() as u64),
        container: Some("avi".into()),
        video_codec: Some("mpeg4".into()),
        audio_codec: Some("mp3".into()),
        video_3d_format: Some(Video3DFormat::HalfSideBySide),
        ..Default::default()
    };
    stores
        .put(MediaItem {
            id: 7,
            path: primary,
            title: "Film".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1280),
                height: Some(720),
                bitrate_bps: Some(1_000_000),
                size_bytes: Some(PRIMARY_BYTES.len() as u64),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                alternate_sources: vec![AlternateMediaSource {
                    id: "3d-hsbs".into(),
                    path: edition,
                    container: edition_probe.container.clone(),
                    video_codec: edition_probe.video_codec.clone(),
                    audio_codec: edition_probe.audio_codec.clone(),
                    bitrate_bps: edition_probe.bitrate_bps,
                    size_bytes: edition_probe.size_bytes,
                    duration_ms: edition_probe.duration_ms,
                    name: Some("3D HSBS".into()),
                    video_3d_format: edition_probe.video_3d_format,
                    probe: Some(Box::new(edition_probe)),
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

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

async fn playback_info(state: &web::Data<AppState>, token: &str, extra: &str) -> serde_json::Value {
    let app = app!(state);
    let req = test::TestRequest::post()
        .uri("/Items/7/PlaybackInfo")
        .insert_header(("X-Emby-Token", token))
        .insert_header(("content-type", "application/json"))
        .set_payload(format!("{{{PROFILE}{extra}}}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    serde_json::from_slice(&body).unwrap()
}

#[actix_web::test]
async fn static_stream_serves_the_selected_editions_file() {
    let td = TempDir::new().unwrap();
    let (state, token) = setup(td.path()).await;
    let app = app!(state);
    let get = |qs: String| {
        test::TestRequest::get()
            .uri(&format!("/Videos/7/stream?Static=true&api_key={token}{qs}"))
            .to_request()
    };

    let body = test::call_and_read_body(&app, get(String::new())).await;
    assert_eq!(&body[..], PRIMARY_BYTES);
    let body =
        test::call_and_read_body(&app, get(format!("&MediaSourceId={}", edition_id()))).await;
    assert_eq!(&body[..], EDITION_BYTES);
    let body = test::call_and_read_body(&app, get(format!("&mediaSourceId={}", wire()))).await;
    assert_eq!(
        &body[..],
        PRIMARY_BYTES,
        "the primary's own id is the primary"
    );
    // A stale edition id (removed by a rescan) still plays, as the primary.
    let body =
        test::call_and_read_body(&app, get(format!("&MediaSourceId={}-extended", wire()))).await;
    assert_eq!(&body[..], PRIMARY_BYTES);
}

#[actix_web::test]
async fn playback_info_for_an_edition_answers_with_that_edition_alone() {
    let td = TempDir::new().unwrap();
    let (state, token) = setup(td.path()).await;

    let all = playback_info(&state, &token, "").await;
    let sources = all["MediaSources"].as_array().unwrap();
    assert_eq!(sources.len(), 2, "{all}");
    assert_eq!(sources[0]["Id"], wire());
    assert_eq!(sources[0]["SupportsDirectPlay"], true, "{all}");
    assert_eq!(sources[1]["Id"], edition_id());

    let one = playback_info(
        &state,
        &token,
        &format!(r#","MediaSourceId":"{}""#, edition_id()),
    )
    .await;
    let sources = one["MediaSources"].as_array().unwrap();
    assert_eq!(sources.len(), 1, "{one}");
    let src = &sources[0];
    assert_eq!(src["Id"], edition_id());
    assert_eq!(src["Name"], "3D HSBS");
    assert_eq!(src["RunTimeTicks"], 30_000u64 * 10_000);
    assert_eq!(src["Video3DFormat"], "HalfSideBySide", "{one}");
    // mpeg4/mp3 in avi does not direct play against this profile: negotiated
    // against the edition's probe, not the primary's mp4.
    assert_eq!(src["SupportsDirectPlay"], false, "{one}");
    let url = src["TranscodingUrl"].as_str().expect("a transcode url");
    assert!(
        url.contains(&format!("MediaSourceId={}", edition_id())),
        "the transcode must read the edition: {url}"
    );
}

#[actix_web::test]
async fn hls_playlists_follow_the_selected_edition() {
    let td = TempDir::new().unwrap();
    let (state, token) = setup(td.path()).await;
    let app = app!(state);
    let playlist = |qs: String| {
        test::TestRequest::get()
            .uri(&format!("/videos/7/main.m3u8?api_key={token}{qs}"))
            .to_request()
    };
    let primary = test::call_and_read_body(&app, playlist(String::new())).await;
    let primary = std::str::from_utf8(&primary).unwrap().to_owned();
    let edition =
        test::call_and_read_body(&app, playlist(format!("&MediaSourceId={}", edition_id()))).await;
    let edition = std::str::from_utf8(&edition).unwrap().to_owned();

    let segments = |p: &str| p.matches("#EXTINF").count();
    assert!(
        segments(&primary) > segments(&edition),
        "60 s vs 30 s:\n{primary}\n---\n{edition}"
    );
    let seg_lines: Vec<&str> = edition
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect();
    assert!(!seg_lines.is_empty(), "{edition}");
    for l in seg_lines {
        assert!(
            l.contains(&format!("MediaSourceId={}", edition_id())),
            "segment url drops the edition: {l}"
        );
        assert!(
            l.contains("s="),
            "edition segments need their own cache generation: {l}"
        );
    }
    assert!(!primary.contains("MediaSourceId="), "{primary}");
}

#[actix_web::test]
async fn an_editions_subtitles_are_its_own() {
    let td = TempDir::new().unwrap();
    let (state, token) = setup(td.path()).await;
    tokio::fs::write(
        td.path().join("Film.en.vtt"),
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nprimary cue\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        td.path().join("Film - 3D.HSBS.en.vtt"),
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nedition cue\n",
    )
    .await
    .unwrap();

    let one = playback_info(
        &state,
        &token,
        &format!(r#","MediaSourceId":"{}""#, edition_id()),
    )
    .await;
    let url = one["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|s| {
            (s["Type"] == "Subtitle")
                .then(|| s["DeliveryUrl"].as_str())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no subtitle delivery url: {one}"))
        .to_owned();
    assert!(
        url.starts_with(&format!("/Videos/{}/{}/Subtitles/", wire(), edition_id())),
        "{url}"
    );

    let app = app!(state);
    let req = test::TestRequest::get()
        .uri(&format!("{url}?api_key={token}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("edition cue"), "{body}");
}
