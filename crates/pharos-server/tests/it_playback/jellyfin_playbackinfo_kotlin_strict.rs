#![allow(clippy::unwrap_used, clippy::expect_used)]
//! PlaybackInfo responses must satisfy jellyfin-sdk-kotlin's STRICT models.
//!
//! The native Android / Android TV apps deserialize with kotlinx.serialization,
//! which throws on ANY missing non-defaulted field — the whole response fails
//! and the app shows "Unable to resolve playback info". jellyfin-web (JS) is
//! lenient, so these gaps are invisible in browser testing. The required-field
//! lists below are copied from the generated kotlin models:
//!   MediaSourceInfo.kt — Protocol, Type, IsRemote, ReadAtNativeFramerate,
//!     IgnoreDts, IgnoreIndex, GenPtsInput, SupportsTranscoding,
//!     SupportsDirectStream, SupportsDirectPlay, IsInfiniteStream,
//!     RequiresOpening, RequiresClosing, RequiresLooping, SupportsProbing,
//!     TranscodingSubProtocol (non-nullable enum!), HasSegments
//!   MediaStream.kt — Type, Index, IsDefault, IsForced, IsHearingImpaired,
//!     IsOriginal, IsInterlaced, IsExternal, IsTextSubtitleStream,
//!     SupportsExternalStream

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SeriesInfo, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore, Video3DFormat,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// Every field jellyfin-sdk-kotlin requires on MediaSourceInfo (no default).
const MEDIA_SOURCE_REQUIRED: &[&str] = &[
    "Protocol",
    "Type",
    "IsRemote",
    "ReadAtNativeFramerate",
    "IgnoreDts",
    "IgnoreIndex",
    "GenPtsInput",
    "SupportsTranscoding",
    "SupportsDirectStream",
    "SupportsDirectPlay",
    "IsInfiniteStream",
    "RequiresOpening",
    "RequiresClosing",
    "RequiresLooping",
    "SupportsProbing",
    "TranscodingSubProtocol",
    "HasSegments",
];

/// Every field jellyfin-sdk-kotlin requires on MediaStream (no default).
const MEDIA_STREAM_REQUIRED: &[&str] = &[
    "Type",
    "Index",
    "IsDefault",
    "IsForced",
    "IsHearingImpaired",
    "IsOriginal",
    "IsInterlaced",
    "IsExternal",
    "IsTextSubtitleStream",
    "SupportsExternalStream",
];

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
    // 10-bit HEVC + EAC3 in MKV — the library's dominant shape and a case the
    // phone will DirectPlay natively / transcode in a browser.
    stores
        .put(MediaItem {
            id: 42,
            path: "/m/42.mkv".into(),
            title: "Strict".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(5_000_000),
                container: Some("matroska,webm".into()),
                video_codec: Some("hevc".into()),
                audio_codec: Some("eac3".into()),
                audio_channels: Some(6),
                subtitle_tracks: vec![pharos_core::SubtitleTrack {
                    stream_index: 2,
                    codec: Some("subrip".into()),
                    ..Default::default()
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

fn assert_required(obj: &serde_json::Value, required: &[&str], what: &str) {
    for key in required {
        let v = obj.get(*key);
        assert!(
            v.is_some(),
            "{what}: kotlin-required field {key} MISSING — native apps fail \
             the whole response. Object: {obj}"
        );
        assert!(
            !v.unwrap().is_null(),
            "{what}: kotlin-required field {key} is NULL (non-nullable in the \
             SDK model). Object: {obj}"
        );
    }
}

#[actix_web::test]
async fn playback_info_satisfies_kotlin_sdk_required_fields() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // The native app's shape: camelCase query params, minimal JSON body.
    let req = test::TestRequest::post()
        .uri("/Items/42/PlaybackInfo?userId=abc&maxStreamingBitrate=20000000&autoOpenLiveStream=true")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let sources = v["MediaSources"].as_array().expect("MediaSources array");
    assert!(!sources.is_empty(), "at least one MediaSource");
    for src in sources {
        assert_required(src, MEDIA_SOURCE_REQUIRED, "MediaSourceInfo");
        let streams = src["MediaStreams"].as_array().expect("MediaStreams");
        assert!(!streams.is_empty(), "at least one MediaStream");
        for s in streams {
            assert_required(s, MEDIA_STREAM_REQUIRED, "MediaStream");
        }
        // Item ids must be GUID-shaped: jellyfin-android's WebView→native
        // bridge parses ids with toUUIDOrNull() and silently drops non-UUID
        // ids — a decimal id empties the play queue (B15).
        let sid = src["Id"].as_str().unwrap();
        assert!(
            sid.len() == 32 && sid.bytes().all(|b| b.is_ascii_hexdigit()),
            "MediaSource Id must be a dashless GUID, got {sid}"
        );
        // Attachments (fonts): Index is the SDK's only required field.
        if let Some(atts) = src["MediaAttachments"].as_array() {
            for a in atts {
                assert_required(a, &["Index"], "MediaAttachment");
            }
        }
    }
}

#[actix_web::test]
async fn playback_info_and_item_dto_serialize_video_3d_format_strictly() {
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

    let items_spec = [
        (
            101_u64,
            MediaKind::Movie,
            Video3DFormat::HalfSideBySide,
            "HalfSideBySide",
        ),
        (102_u64, MediaKind::Movie, Video3DFormat::Mvc, "MVC"),
        (
            103_u64,
            MediaKind::Episode,
            Video3DFormat::HalfTopAndBottom,
            "HalfTopAndBottom",
        ),
    ];

    for (id, kind, fmt, _wire) in &items_spec {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: format!("Title {id}"),
                kind: *kind,
                series: if *kind == MediaKind::Episode {
                    Some(SeriesInfo {
                        series_name: "Show".into(),
                        season_number: Some(1),
                        episode_number: Some(1),
                        series_folder: None,
                        series_year: None,
                    })
                } else {
                    None
                },
                probe: MediaProbe {
                    duration_ms: Some(60_000),
                    width: Some(1920),
                    height: Some(1080),
                    bitrate_bps: Some(5_000_000),
                    container: Some("matroska,webm".into()),
                    video_codec: Some("h264".into()),
                    audio_codec: Some("aac".into()),
                    audio_channels: Some(2),
                    video_3d_format: Some(*fmt),
                    subtitle_tracks: vec![pharos_core::SubtitleTrack {
                        stream_index: 2,
                        codec: Some("subrip".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }

    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let token_str = token.0.expose().to_string();

    for (id, _kind, _fmt, wire) in &items_spec {
        // 1. POST /Items/{id}/PlaybackInfo
        let req = test::TestRequest::post()
            .uri(&format!(
                "/Items/{id}/PlaybackInfo?userId={uid}&maxStreamingBitrate=20000000&autoOpenLiveStream=true"
            ))
            .insert_header(("X-Emby-Token", token_str.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload("{}")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body = test::read_body(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let sources = v["MediaSources"].as_array().expect("MediaSources array");
        assert!(!sources.is_empty(), "at least one MediaSource");
        for src in sources {
            assert_required(src, MEDIA_SOURCE_REQUIRED, "MediaSourceInfo");
            assert_eq!(
                src["Video3DFormat"].as_str(),
                Some(*wire),
                "MediaSourceInfo Video3DFormat must be exact string {wire}"
            );
            assert!(
                src.get("Video3dFormat").is_none() || src["Video3dFormat"].is_null(),
                "MediaSourceInfo Video3dFormat must be absent or null"
            );

            let streams = src["MediaStreams"].as_array().expect("MediaStreams array");
            assert!(!streams.is_empty(), "at least one MediaStream");
            for s in streams {
                assert_required(s, MEDIA_STREAM_REQUIRED, "MediaStream");
                let stream_type = s["Type"].as_str().unwrap_or("");
                if stream_type == "Video" {
                    assert_eq!(
                        s["Video3DFormat"].as_str(),
                        Some(*wire),
                        "Video MediaStream Video3DFormat must match {wire}"
                    );
                    assert!(
                        s.get("Video3dFormat").is_none() || s["Video3dFormat"].is_null(),
                        "Video MediaStream Video3dFormat must be absent or null"
                    );
                } else {
                    assert!(
                        s.get("Video3DFormat").is_none() || s["Video3DFormat"].is_null(),
                        "Non-video stream must not contain Video3DFormat: {s}"
                    );
                    assert!(
                        s.get("Video3dFormat").is_none() || s["Video3dFormat"].is_null(),
                        "Non-video stream must not contain Video3dFormat: {s}"
                    );
                }
            }
        }

        // 2. GET /Users/{user_id}/Items/{id}
        let user_hex = uid.0.simple().to_string();
        let req_user_item = test::TestRequest::get()
            .uri(&format!("/Users/{user_hex}/Items/{id}"))
            .insert_header(("X-Emby-Token", token_str.as_str()))
            .to_request();
        let resp_user_item = test::call_service(&app, req_user_item).await;
        assert_eq!(resp_user_item.status(), 200);
        let item_body = test::read_body(resp_user_item).await;
        let dto: serde_json::Value = serde_json::from_slice(&item_body).unwrap();
        assert_eq!(
            dto["Video3DFormat"].as_str(),
            Some(*wire),
            "BaseItemDto Video3DFormat must match {wire}"
        );
        assert!(
            dto.get("Video3dFormat").is_none() || dto["Video3dFormat"].is_null(),
            "BaseItemDto Video3dFormat must be absent or null"
        );

        // 3. GET /Items/{id}
        let req_item = test::TestRequest::get()
            .uri(&format!("/Items/{id}"))
            .insert_header(("X-Emby-Token", token_str.as_str()))
            .to_request();
        let resp_item = test::call_service(&app, req_item).await;
        assert_eq!(resp_item.status(), 200);
        let item_body2 = test::read_body(resp_item).await;
        let dto2: serde_json::Value = serde_json::from_slice(&item_body2).unwrap();
        assert_eq!(
            dto2["Video3DFormat"].as_str(),
            Some(*wire),
            "BaseItemDto Video3DFormat via /Items/{id} must match {wire}"
        );
    }
}
