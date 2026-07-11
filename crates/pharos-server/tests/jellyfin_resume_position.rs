#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P4 — PlaybackInfo emits resume offset.
//!
//! Real Jellyfin clients (Finamp, Android-TV) drive playback from
//! `/Items/{id}/PlaybackInfo` without re-fetching `BaseItemDto`.
//! `StartPositionTicks` lands on both the MediaSource and the
//! top-level response so either-shape reader picks it up.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{self, items},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed(played: bool, position_ticks: u64) -> (web::Data<AppState>, String) {
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
            id: 7,
            path: "/fake/path.mkv".into(),
            title: "Resume Test".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_600_000), // 1h
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(5_000_000),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    stores
        .set_user_data(
            uid,
            7,
            UserItemData {
                played,
                play_count: 0,
                last_played_position_ticks: position_ticks,
                is_favorite: false,
                last_played_at: 0,
            },
        )
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
async fn playback_info_emits_resume_position_on_both_top_and_media_source() {
    let (state, token) = seed(false, 12_000_000_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/Items/7/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"DeviceProfile":{}}"#)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["StartPositionTicks"].as_u64(),
        Some(12_000_000_000),
        "top-level missing or wrong: {v}"
    );
    assert_eq!(
        v["MediaSources"][0]["StartPositionTicks"].as_u64(),
        Some(12_000_000_000),
        "MediaSource missing: {v}"
    );
}

#[actix_web::test]
async fn playback_info_emits_zero_resume_when_item_is_played() {
    // Jellyfin convention: played=true means "watched already";
    // resume offset gets zeroed so playback restarts from 0.
    let (state, token) = seed(true, 12_000_000_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/Items/7/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"DeviceProfile":{}}"#)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["StartPositionTicks"].as_u64(), Some(0), "{v}");
    assert_eq!(
        v["MediaSources"][0]["StartPositionTicks"].as_u64(),
        Some(0),
        "{v}"
    );
}

#[actix_web::test]
async fn playback_info_emits_zero_when_no_user_data_row_exists() {
    // Items.rs.get_user_data returns default (zeros) for missing rows;
    // playback_info should treat that as "no resume".
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
            path: "/fake/never-played.mkv".into(),
            title: "Never Played".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe::default(),
            series: None,
            created_at: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));

    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Items/9/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"DeviceProfile":{}}"#)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["StartPositionTicks"].as_u64(), Some(0), "{v}");
}

// Silence unused-import warning on `items` — it's used only at compile-
// time for the `playback_info` test surface but rustc doesn't see it.
#[allow(dead_code)]
fn _items_surface_ref() -> fn(&mut actix_web::web::ServiceConfig) {
    items::register
}

/// The three home rows (Continue Watching / Listening / Reading) all hit
/// `/Users/{id}/Items/Resume`, distinguished only by `MediaTypes`. The endpoint
/// must honour it, else e.g. a movie shows up under "Continue Reading".
#[actix_web::test]
async fn resume_list_filters_by_media_types() {
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
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();
    // One video (Movie) + one audio item, both mid-playback (resumable).
    for (id, kind, title) in [
        (10u64, MediaKind::Movie, "A Movie"),
        (11u64, MediaKind::Audio, "An Album"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}").into(),
                title: title.into(),
                kind,
                probe: MediaProbe {
                    duration_ms: Some(3_600_000),
                    ..Default::default()
                },
                series: None,
                created_at: None,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        stores
            .set_user_data(
                uid,
                id,
                UserItemData {
                    played: false,
                    play_count: 0,
                    last_played_position_ticks: 60_000_000_000,
                    is_favorite: false,
                    last_played_at: 0,
                },
            )
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let uid_str = uid.0.simple().to_string();

    let titles = |v: &serde_json::Value| -> Vec<String> {
        v["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["Name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let get = |mt: &str| {
        let uri = format!("/Users/{uid_str}/Items/Resume?MediaTypes={mt}");
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request()
    };

    let video: serde_json::Value =
        serde_json::from_slice(&test::call_and_read_body(&app, get("Video")).await).unwrap();
    assert_eq!(
        titles(&video),
        vec!["A Movie"],
        "Video row = the movie only"
    );

    let audio: serde_json::Value =
        serde_json::from_slice(&test::call_and_read_body(&app, get("Audio")).await).unwrap();
    assert_eq!(
        titles(&audio),
        vec!["An Album"],
        "Audio row = the album only"
    );

    let book: serde_json::Value =
        serde_json::from_slice(&test::call_and_read_body(&app, get("Book")).await).unwrap();
    assert!(
        titles(&book).is_empty(),
        "Book row must be empty (pharos has no book media): {:?}",
        titles(&book)
    );
}
