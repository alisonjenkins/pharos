#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P12 — `DefaultSubtitleStreamIndex` resolution.
//!
//! Priority: the client's explicit pick → the user's `SubtitleMode` → None.
//!
//! The ladder this once pinned (is_default → first English → ANY track) was
//! pharos', not Jellyfin's, and its last two rungs turned subtitles ON for
//! titles nobody asked them for: a container carrying a single incidental
//! English track always selected it. Selection now runs Jellyfin's
//! `SubtitleMode`, configured from stock jellyfin-web, whose Default mode
//! picks only external/default/forced tracks — so an unconfigured user gets
//! subtitles when the CONTAINER says so and not otherwise. `Always` is the
//! mode that restores "give me the English track"; `Smart` gives it only when
//! the audio is in a language the user does not read.

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
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
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
    // The container states this one, so every mode but None honours it —
    // B44's forced track and B104's default ASS still select, and still burn.
    assert_eq!(fetch_default(state, token).await, Some(3));
}

#[actix_web::test]
async fn leaves_an_incidental_english_track_off_under_the_default_mode() {
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
    // Neither track is external, default or forced, so Jellyfin's Default mode
    // selects nothing — the old ladder picked the English one and switched
    // subtitles on unbidden.
    assert_eq!(fetch_default(state, token).await, None);
}

#[actix_web::test]
async fn does_not_fall_back_to_an_arbitrary_track() {
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
    // A lone non-default, non-forced track is not a statement that it should
    // play; the old ladder's final rung treated it as one.
    assert_eq!(fetch_default(state, token).await, None);
}

#[actix_web::test]
async fn none_when_no_subtitle_tracks() {
    let (state, token) = seed(vec![]).await;
    assert_eq!(fetch_default(state, token).await, None);
}
