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
