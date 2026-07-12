//! Wire-shape invariants — checks that catch the bug shapes we kept
//! shipping past unit tests + Playwright happy-path coverage:
//!
//! 1. Synthesise vs reality drift — DTOs must never advertise a
//!    stream / codec the underlying MediaItem doesn't actually have.
//!
//! 2. Container alias consistency — when ffprobe reports a comma-
//!    joined list ("matroska,webm"), `Container` in the DTO must
//!    match one of jellyfin-web's DirectPlayProfile tokens.
//!
//! 3. DirectPlay vs transcode field consistency — when
//!    SupportsDirectPlay is true, TranscodingSubProtocol must be
//!    `None` (otherwise jellyfin-web routes the URL through hls.js).
//!
//! 4. ID round-trips — synthesised library / series / season ids are
//!    32-hex-char strings and routing for them works via /Items/{id}.
//!
//! 5. PascalCase wire shape — every BaseItemDto field clients rely on
//!    serialises with PascalCase keys (no camelCase leak).
//!
//! These are property-style assertions across small fixture grids
//! rather than per-endpoint unit checks — they catch the bug
//! categories the existing tests miss.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SeriesInfo, SubtitleTrack,
    TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_with_items(items: Vec<MediaItem>) -> (web::Data<AppState>, String) {
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
    for item in items {
        stores.put(item).await.unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

fn build_app(
    state: web::Data<AppState>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

/// (1) — When MediaProbe.audio_codec is None, MediaSource.MediaStreams
/// must NOT carry an Audio entry. Caught the BBB / AAC fabrication
/// bug that broke direct play in jellyfin-web.
#[actix_web::test]
async fn silent_video_emits_no_audio_stream_in_playback_info() {
    let probe = MediaProbe {
        video_codec: Some("vp9".into()),
        width: Some(1920),
        height: Some(1080),
        // audio_codec deliberately None
        ..Default::default()
    };
    let (state, token) = seed_with_items(vec![MediaItem {
        id: 1,
        path: "/m/silent.webm".into(),
        title: "silent".into(),
        kind: MediaKind::Movie,
        probe,
        series: None,
        created_at: None,
        metadata: Default::default(),
    }])
    .await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Items/1/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let streams = v["MediaSources"][0]["MediaStreams"].as_array().unwrap();
    let audio_count = streams.iter().filter(|s| s["Type"] == "Audio").count();
    assert_eq!(audio_count, 0, "fabricated audio stream: {streams:?}");
}

/// (2) — When ffprobe reports "matroska,webm" the wire Container must
/// be one of the comma-separated tokens (not the literal joined
/// string). The fix prefers `webm` over `matroska` because that's
/// what jellyfin-web's DirectPlayProfile lists.
#[actix_web::test]
async fn container_alias_is_a_single_known_token() {
    let probe = MediaProbe {
        container: Some("matroska,webm".into()),
        video_codec: Some("vp9".into()),
        ..Default::default()
    };
    let (state, token) = seed_with_items(vec![MediaItem {
        id: 2,
        path: "/m/x.webm".into(),
        title: "x".into(),
        kind: MediaKind::Movie,
        probe,
        series: None,
        created_at: None,
        metadata: Default::default(),
    }])
    .await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Items/2/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let container = v["MediaSources"][0]["Container"].as_str().unwrap();
    assert!(
        ["webm", "matroska", "mkv"].contains(&container),
        "container={container} (must be a single token, not the alias list)"
    );
    // Prefer webm specifically because that's what DirectPlayProfile
    // lists for vp9 sources.
    assert_eq!(container, "webm");
}

/// (3) — When SupportsDirectPlay is true (and we picked the direct-
/// play path), TranscodingSubProtocol must be "http" — NEVER "hls",
/// and NEVER null/omitted:
/// - "hls" alongside SupportsDirectPlay=true sent jellyfin-web's video
///   player through hls.js, which threw `manifestParsingError` on the
///   direct-play webm bytes (the original invariant).
/// - null/omitted fails jellyfin-sdk-kotlin outright: the SDK's
///   MediaSourceInfo marks TranscodingSubProtocol as a REQUIRED
///   non-nullable enum, so the native Android/TV apps reject the whole
///   PlaybackInfo response ("Unable to resolve playback info" — B13).
/// Real Jellyfin emits "http" here; jellyfin-web only routes through
/// hls.js when a TranscodingUrl is actually present.
#[actix_web::test]
async fn direct_play_omits_transcoding_sub_protocol() {
    let probe = MediaProbe {
        container: Some("webm".into()),
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        ..Default::default()
    };
    let (state, token) = seed_with_items(vec![MediaItem {
        id: 3,
        path: "/m/x.webm".into(),
        title: "x".into(),
        kind: MediaKind::Movie,
        probe,
        series: None,
        created_at: None,
        metadata: Default::default(),
    }])
    .await;
    let app = test::init_service(build_app(state)).await;
    // Use a realistic profile that accepts vp9/opus webm direct.
    let req = test::TestRequest::post()
        .uri("/Items/3/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload(
            r#"{"DeviceProfile":{
                "MaxStreamingBitrate":120000000,
                "DirectPlayProfiles":[{"Container":"webm","Type":"Video","VideoCodec":"vp9","AudioCodec":"opus"}]
            }}"#,
        )
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let src = &v["MediaSources"][0];
    assert_eq!(src["SupportsDirectPlay"], true, "{src:?}");
    assert_eq!(
        src["TranscodingSubProtocol"], "http",
        "DirectPlay advertises sub-protocol 'http' — never 'hls' (would route \
         via hls.js) and never null (kotlin SDK requires the field): {src:?}"
    );
    assert!(
        src.get("TranscodingUrl").map_or(true, |t| t.is_null()),
        "DirectPlay must not advertise a TranscodingUrl: {src:?}"
    );
}

/// (4) — Synthesised library / series / season ids are 32 hex chars
/// (matches the uuid-shaped regex jellyfin-web uses) AND requesting
/// /Items/{id} for them returns a CollectionFolder / Series / Season
/// shape rather than 400-ing on u64::parse.
#[actix_web::test]
async fn synth_ids_route_via_items_endpoint() {
    let probe = MediaProbe::default();
    let items = vec![MediaItem {
        id: 100,
        path: "/m/TV/Show/Season 1/s01e01.mkv".into(),
        title: "S01E01".into(),
        kind: MediaKind::Episode,
        probe,
        series: Some(SeriesInfo {
            series_name: "Show".into(),
            season_number: Some(1),
            episode_number: Some(1),
            ..Default::default()
        }),
        created_at: None,
        metadata: Default::default(),
    }];
    let (state, token) = seed_with_items(items).await;
    // Add a configured root so library ids exist.
    let state = web::Data::new(
        std::sync::Arc::try_unwrap(state.into_inner())
            .unwrap_or_else(|_| panic!("state arc"))
            .with_media_roots(vec!["/m".into()]),
    );
    let app = test::init_service(build_app(state)).await;
    // Walk: /Items/100 → Episode DTO carrying SeriesId + SeasonId.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let series_id = v["SeriesId"].as_str().unwrap();
    let season_id = v["SeasonId"].as_str().unwrap();
    assert_eq!(series_id.len(), 32, "{series_id}");
    assert_eq!(season_id.len(), 32, "{season_id}");
    assert!(series_id.chars().all(|c| c.is_ascii_hexdigit()));
    // Each synth id must resolve via /Items/{id}.
    for (id, expected_type) in [(series_id, "Series"), (season_id, "Season")] {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items/{id}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["Type"], expected_type, "/Items/{id} got {v:?}");
    }
}

/// (5) — Every key in BaseItemDto / MediaSource starts with an
/// uppercase letter. PascalCase regression check; one missing
/// `rename_all` would break jellyfin-web silently.
#[actix_web::test]
async fn item_dto_keys_are_pascal_case() {
    let probe = MediaProbe {
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        ..Default::default()
    };
    let (state, token) = seed_with_items(vec![MediaItem {
        id: 5,
        path: "/m/x.webm".into(),
        title: "x".into(),
        kind: MediaKind::Movie,
        probe,
        series: None,
        created_at: None,
        metadata: Default::default(),
    }])
    .await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/5")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let obj = v.as_object().unwrap();
    for key in obj.keys() {
        let first = key.chars().next().unwrap();
        assert!(
            first.is_ascii_uppercase(),
            "BaseItemDto key `{key}` not PascalCase"
        );
    }
    // Also recurse one layer into MediaSources[0].
    let src = obj
        .get("MediaSources")
        .and_then(|v| v[0].as_object())
        .unwrap();
    for key in src.keys() {
        let first = key.chars().next().unwrap();
        assert!(
            first.is_ascii_uppercase(),
            "MediaSource key `{key}` not PascalCase"
        );
    }
}

/// (6) — Probe-discovered subtitle tracks must appear as Type=Subtitle
/// MediaStream entries with a DeliveryUrl that routes via the
/// /Videos/{id}/.../Subtitles/{index}/Stream.vtt endpoint.
#[actix_web::test]
async fn embedded_subtitle_tracks_surface_with_delivery_url() {
    let probe = MediaProbe {
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        subtitle_tracks: vec![SubtitleTrack {
            stream_index: 2,
            language: Some("eng".into()),
            codec: Some("webvtt".into()),
            title: Some("English".into()),
            is_default: true,
            is_forced: false,
            is_hearing_impaired: false,
        }],
        ..Default::default()
    };
    let (state, token) = seed_with_items(vec![MediaItem {
        id: 6,
        path: "/m/x.mkv".into(),
        title: "x".into(),
        kind: MediaKind::Movie,
        probe,
        series: None,
        created_at: None,
        metadata: Default::default(),
    }])
    .await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/6/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload("{}")
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let streams = v["MediaSources"][0]["MediaStreams"].as_array().unwrap();
    let sub = streams
        .iter()
        .find(|s| s["Type"] == "Subtitle")
        .expect("subtitle stream surfaces");
    assert_eq!(sub["Language"], "eng");
    assert_eq!(sub["IsDefault"], true);
    assert_eq!(sub["IsExternal"], false);
    let url = sub["DeliveryUrl"].as_str().unwrap();
    assert!(url.contains("/Subtitles/2/Stream.vtt"), "{url}");
}

/// (7) — Video items advertise Primary + Backdrop + Thumb ImageTags
/// so jellyfin-web's tile grid renders covers without 404-ing on
/// /Items/{id}/Images/Primary?tag=…. Audio gets Primary only.
#[actix_web::test]
async fn video_item_advertises_primary_backdrop_thumb_image_tags() {
    let (state, token) = seed_with_items(vec![
        MediaItem {
            id: 100,
            path: "/m/movie.mkv".into(),
            title: "Movie".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        },
        MediaItem {
            id: 101,
            path: "/m/song.mp3".into(),
            title: "Song".into(),
            kind: MediaKind::Audio,
            ..Default::default()
        },
    ])
    .await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tags = v["ImageTags"].as_object().expect("ImageTags object");
    assert!(tags.contains_key("Primary"), "{tags:?}");
    assert!(tags.contains_key("Backdrop"), "{tags:?}");
    assert!(tags.contains_key("Thumb"), "{tags:?}");
    let backdrops = v["BackdropImageTags"].as_array().unwrap();
    assert_eq!(backdrops.len(), 1);

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/101")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tags = v["ImageTags"].as_object().expect("ImageTags object");
    assert!(tags.contains_key("Primary"));
    // Audio doesn't get Backdrop / Thumb (no frame to grab).
    assert!(!tags.contains_key("Backdrop"));
    assert!(!tags.contains_key("Thumb"));
}

/// (9) — Every item DTO must carry a ParentId so jellyfin-web's
/// breadcrumb back-nav works. Episode → SeasonId; audio with album
/// → AlbumId; otherwise the library root id matching the path.
#[actix_web::test]
async fn every_item_has_a_parent_id() {
    let (state_inner, token) = seed_with_items(vec![
        MediaItem {
            id: 1,
            path: "/m/Movies/big.mkv".into(),
            title: "movie".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        },
        MediaItem {
            id: 2,
            path: "/m/TV/Show/Season 1/s01e01.mkv".into(),
            title: "ep".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        },
    ])
    .await;
    // Inject media_roots so the library_parent_id pass has something
    // to match.
    let state = web::Data::new(
        std::sync::Arc::try_unwrap(state_inner.into_inner())
            .unwrap_or_else(|_| panic!("state arc"))
            .with_media_roots(vec!["/m/Movies".into(), "/m/TV".into()]),
    );
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    for it in v["Items"].as_array().unwrap() {
        let pid = it["ParentId"].as_str();
        assert!(
            pid.is_some_and(|p| p.len() == 32),
            "{} has no ParentId: {it:?}",
            it["Name"]
        );
    }
}

/// (10) — Episode promotion. A path that matches the TV-layout
/// heuristic must yield kind=Episode in the put → list → get
/// round-trip (catches scanner classification regressions across
/// the storage layer).
#[tokio::test]
async fn episode_classification_survives_store_roundtrip() {
    use pharos_scanner::is_episode_path;
    let path = std::path::Path::new("/m/Show/Season 1/Show.S01E03.mkv");
    assert!(is_episode_path(path));
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    stores
        .put(MediaItem {
            id: 50,
            path: path.to_path_buf(),
            title: "Show.S01E03".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Show".into(),
                season_number: Some(1),
                episode_number: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let got = MediaStore::get(&stores, 50).await.unwrap();
    assert!(matches!(got.kind, MediaKind::Episode));
    let series = got.series.expect("series persisted");
    assert_eq!(series.series_name, "Show");
    assert_eq!(series.season_number, Some(1));
    assert_eq!(series.episode_number, Some(3));
}

/// (11) — /Library/MediaFolders and /Users/{u}/Views must emit
/// identical Items (same Id + Name) so jellyfin-web's library nav
/// resolves the same id from either entry point. The two endpoints
/// share `library_views()` internally — this is the regression net.
#[actix_web::test]
async fn library_mediafolders_and_user_views_emit_identical_items() {
    use pharos_core::UserId;
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
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec!["/m/Movies".into(), "/m/TV".into()]),
    );
    let app = test::init_service(build_app(state)).await;
    let by_views: serde_json::Value = serde_json::from_slice(
        &test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Users/{}/Views", uid.0.simple()))
                .insert_header(("X-Emby-Token", token.0.expose()))
                .to_request(),
        )
        .await,
    )
    .unwrap();
    let by_folders: serde_json::Value = serde_json::from_slice(
        &test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri("/Library/MediaFolders")
                .insert_header(("X-Emby-Token", token.0.expose()))
                .to_request(),
        )
        .await,
    )
    .unwrap();
    // Both should produce two items with stable hash ids.
    let extract = |v: &serde_json::Value| -> Vec<(String, String)> {
        v["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["Id"].as_str().unwrap().to_string(),
                    x["Name"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let mut v_views = extract(&by_views);
    let mut v_folders = extract(&by_folders);
    v_views.sort();
    v_folders.sort();
    assert_eq!(v_views, v_folders, "Views vs MediaFolders mismatch");
}
