#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P12 — `DefaultSubtitleStreamIndex` resolution.
//!
//! Priority: is_default → English → first track → None.

use actix_web::{test, web, App};
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

async fn seed(tracks: Vec<SubtitleTrack>) -> (web::Data<AppState>, String) {
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
            id: 9,
            path: "/no/such.mkv".into(),
            title: "m".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                subtitle_tracks: tracks,
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

async fn fetch_default(state: web::Data<AppState>, token: String) -> Option<u32> {
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Items/9/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"DeviceProfile":{}}"#)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["MediaSources"][0]["DefaultSubtitleStreamIndex"]
        .as_u64()
        .map(|n| n as u32)
}

#[actix_web::test]
async fn picks_default_flagged_track() {
    let tracks = vec![
        SubtitleTrack {
            stream_index: 2,
            language: Some("jpn".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
        SubtitleTrack {
            stream_index: 3,
            language: Some("fra".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: true,
            is_forced: false,
            is_hearing_impaired: false,
        },
    ];
    let (state, token) = seed(tracks).await;
    assert_eq!(fetch_default(state, token).await, Some(3));
}

#[actix_web::test]
async fn picks_english_when_no_default() {
    let tracks = vec![
        SubtitleTrack {
            stream_index: 2,
            language: Some("jpn".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
        SubtitleTrack {
            stream_index: 3,
            language: Some("eng".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
    ];
    let (state, token) = seed(tracks).await;
    assert_eq!(fetch_default(state, token).await, Some(3));
}

#[actix_web::test]
async fn picks_first_track_when_no_default_and_no_english() {
    let tracks = vec![
        SubtitleTrack {
            stream_index: 2,
            language: Some("jpn".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
        SubtitleTrack {
            stream_index: 3,
            language: Some("fra".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
    ];
    let (state, token) = seed(tracks).await;
    assert_eq!(fetch_default(state, token).await, Some(2));
}

#[actix_web::test]
async fn none_when_no_subtitle_tracks() {
    let (state, token) = seed(vec![]).await;
    assert_eq!(fetch_default(state, token).await, None);
}
