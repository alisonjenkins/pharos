#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 3D versions on the Jellyfin wire: `Is3D` filtering plus `Is3D` /
//! `Video3DFormat` on items and on each `MediaSourceInfo`, with the exact
//! casing jellyfin-web and the SDKs read (`Is3D`, `Video3DFormat`).

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

fn movie(id: u64, title: &str, probe: MediaProbe) -> MediaItem {
    MediaItem {
        id,
        path: format!("/m/{id}.mkv").into(),
        title: title.into(),
        kind: MediaKind::Movie,
        probe: MediaProbe {
            container: Some("matroska".into()),
            video_codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            ..probe
        },
        ..Default::default()
    }
}

/// 1 = plain 2D, 2 = a lone 3D rip, 3 = 2D movie with a 3D edition.
async fn seed() -> (web::Data<AppState>, String) {
    seed_with(Vec::new()).await
}

async fn seed_with(extra: Vec<MediaItem>) -> (web::Data<AppState>, String) {
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
    stores
        .put(movie(1, "Plain", MediaProbe::default()))
        .await
        .unwrap();
    stores
        .put(movie(
            2,
            "Lone 3D",
            MediaProbe {
                video_3d_format: Some(Video3DFormat::HalfTopAndBottom),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    stores
        .put(movie(
            3,
            "Has 3D Edition",
            MediaProbe {
                alternate_sources: vec![AlternateMediaSource {
                    id: "3d-hsbs".into(),
                    path: "/m/3-3d.mkv".into(),
                    name: Some("3D HSBS".into()),
                    video_3d_format: Some(Video3DFormat::HalfSideBySide),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    for item in extra {
        stores.put(item).await.unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

macro_rules! app {
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

macro_rules! get_json {
    ($app:expr, $token:expr, $uri:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("X-Emby-Token", $token.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "GET {}", $uri);
        serde_json::from_slice::<serde_json::Value>(&test::read_body(resp).await).unwrap()
    }};
}

fn names(v: &serde_json::Value) -> Vec<String> {
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap().to_string())
        .collect()
}

#[actix_web::test]
async fn is_3d_filter_matches_a_3d_file_or_a_3d_edition() {
    let (state, token) = seed().await;
    let app = app!(state);
    // camelCase, as the SDK sends it.
    let only_3d = get_json!(app, token, "/Items?is3D=true&sortBy=SortName");
    assert_eq!(names(&only_3d), vec!["Has 3D Edition", "Lone 3D"]);
    let only_2d = get_json!(app, token, "/Items?Is3D=false");
    assert_eq!(names(&only_2d), vec!["Plain"]);
}

#[actix_web::test]
async fn items_carry_is_3d_and_format_only_when_3d() {
    let (state, token) = seed().await;
    let app = app!(state);
    let plain = get_json!(app, token, "/Items/1");
    assert!(plain.get("Is3D").is_none(), "2D payload must not grow");
    assert!(plain.get("Video3DFormat").is_none());

    let lone = get_json!(app, token, "/Items/2");
    assert_eq!(lone["Is3D"], true);
    assert_eq!(lone["Video3DFormat"], "HalfTopAndBottom");

    // 3D edition only: the item is findable as 3D, but its own file is 2D.
    let with_edition = get_json!(app, token, "/Items/3");
    assert_eq!(with_edition["Is3D"], true);
    assert!(with_edition.get("Video3DFormat").is_none());
}

#[actix_web::test]
async fn playback_info_labels_each_source_with_its_3d_format() {
    let (state, token) = seed().await;
    let app = app!(state);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Items/3/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload("{}")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let sources = v["MediaSources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert!(sources[0].get("Video3DFormat").is_none());
    assert_eq!(sources[1]["Name"], "3D HSBS");
    assert_eq!(sources[1]["Video3DFormat"], "HalfSideBySide");
}

/// 10 = Blu-ray 3D rip, 11 = half side-by-side rip; both h264/aac in Matroska.
fn transcode_fixtures() -> Vec<MediaItem> {
    [
        (10, "Bluray 3D", Video3DFormat::Mvc),
        (11, "Packed HSBS", Video3DFormat::HalfSideBySide),
    ]
    .into_iter()
    .map(|(id, title, fmt)| {
        movie(
            id,
            title,
            MediaProbe {
                audio_codec: Some("aac".into()),
                video_3d_format: Some(fmt),
                ..Default::default()
            },
        )
    })
    .collect()
}

const MKV_PLAYER: &str = r#""DeviceProfile":{
  "DirectPlayProfiles":[{"Type":"Video","Container":"mkv","VideoCodec":"h264","AudioCodec":"aac"}],
  "TranscodingProfiles":[{"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}]
}"#;

const MP4_ONLY_PLAYER: &str = r#""DeviceProfile":{
  "DirectPlayProfiles":[{"Type":"Video","Container":"mp4","VideoCodec":"h264","AudioCodec":"aac"}],
  "TranscodingProfiles":[{"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}]
}"#;

/// The first MediaSource of `item`'s PlaybackInfo for `profile`.
macro_rules! playback_source {
    ($app:expr, $token:expr, $item:expr, $query:expr, $profile:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::post()
                .uri(&format!("/Items/{}/PlaybackInfo{}", $item, $query))
                .insert_header(("X-Emby-Token", $token.as_str()))
                .insert_header(("content-type", "application/json"))
                .set_payload(format!("{{{}}}", $profile))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
        v["MediaSources"][0].clone()
    }};
}

fn video_stream(source: &serde_json::Value) -> &serde_json::Value {
    source["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["Type"] == "Video")
        .unwrap()
}

/// B290 / V233. A transcode of Blu-ray 3D is 2D (ffmpeg decodes only the
/// base view), so the source must not claim MVC. Disarm check: advertising
/// `probe.video_3d_format` unconditionally fails the assertions.
#[actix_web::test]
async fn mvc_transcode_advertises_no_3d_format() {
    let (state, token) = seed_with(transcode_fixtures()).await;
    let app = app!(state);
    let src = playback_source!(app, token, 10, "", MP4_ONLY_PLAYER);
    assert_eq!(src["SupportsDirectPlay"], false);
    assert!(src["TranscodingUrl"].as_str().is_some());
    assert!(src.get("Video3DFormat").is_none(), "{src}");
    assert!(video_stream(&src).get("Video3DFormat").is_none(), "{src}");
}

#[actix_web::test]
async fn mvc_direct_play_keeps_mvc() {
    let (state, token) = seed_with(transcode_fixtures()).await;
    let app = app!(state);
    let src = playback_source!(app, token, 10, "", MKV_PLAYER);
    assert_eq!(src["SupportsDirectPlay"], true);
    assert_eq!(src["Video3DFormat"], "MVC");
    assert_eq!(video_stream(&src)["Video3DFormat"], "MVC");
}

#[actix_web::test]
async fn packed_transcode_keeps_its_format_unless_flattened_to_mono() {
    let (state, token) = seed_with(transcode_fixtures()).await;
    let app = app!(state);
    let kept = playback_source!(app, token, 11, "", MP4_ONLY_PLAYER);
    assert_eq!(kept["SupportsDirectPlay"], false);
    assert_eq!(kept["Video3DFormat"], "HalfSideBySide");
    assert_eq!(video_stream(&kept)["Video3DFormat"], "HalfSideBySide");

    let mono = playback_source!(app, token, 11, "?StereoMode=mono", MKV_PLAYER);
    assert_eq!(mono["SupportsDirectPlay"], false);
    assert!(mono["TranscodingUrl"]
        .as_str()
        .unwrap()
        .contains("StereoMode=mono"));
    assert!(mono.get("Video3DFormat").is_none(), "{mono}");
    assert!(video_stream(&mono).get("Video3DFormat").is_none(), "{mono}");
}

/// The signal for B290: how often a transcode flattens 3D, and why.
/// Disarm check: dropping the `output` label from the counter fails this.
#[actix_web::test]
async fn the_3d_decision_counter_says_what_the_output_is() {
    let (state, token) = seed_with(transcode_fixtures()).await;
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let app = app!(state);
    let _ = playback_source!(app, token, 10, "", MP4_ONLY_PLAYER);
    let _ = playback_source!(app, token, 11, "?StereoMode=mono", MKV_PLAYER);
    let _ = playback_source!(app, token, 11, "", MP4_ONLY_PLAYER);
    let _ = playback_source!(app, token, 10, "", MKV_PLAYER);
    let count = |output: &str, decision: &str| -> u64 {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_playback_3d_decision_total"
                    && [("output", output), ("decision", decision)]
                        .iter()
                        .all(|(lk, lv)| key.labels().any(|l| l.key() == *lk && l.value() == *lv))
            })
            .map(|(.., v)| match v {
                metrics_util::debugging::DebugValue::Counter(n) => n,
                other => panic!("{other:?}"),
            })
            .sum()
    };
    assert_eq!(count("flattened_mvc", "transcode"), 1);
    assert_eq!(count("flattened_mono", "transcode"), 1);
    assert_eq!(count("preserved", "transcode"), 1);
    assert_eq!(count("preserved", "direct_play"), 1);
}
