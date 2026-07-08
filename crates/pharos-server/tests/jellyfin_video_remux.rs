#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P9 — `Decision::VideoRemux` for container-only mismatches.
//!
//! Negotiator tests cover the decision shape; PlaybackInfo test
//! covers the wire shape (TranscodingUrl + SupportsDirectStream).

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{
        self,
        device_profile::{negotiate, Decision, DeviceProfile, DirectPlayProfile, SourceMedia},
    },
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

fn mp4_h264_aac_profile() -> DeviceProfile {
    DeviceProfile {
        direct_play_profiles: vec![DirectPlayProfile {
            kind: "Video".into(),
            container: "mp4".into(),
            video_codec: "h264".into(),
            audio_codec: "aac".into(),
        }],
        transcoding_profiles: vec![],
        max_streaming_bitrate: None,
        max_static_bitrate: None,
        codec_profiles: vec![],
    }
}

#[::core::prelude::v1::test]
fn mkv_h264_aac_against_mp4_profile_remuxes_container_only() {
    let source = SourceMedia {
        container: "matroska".into(),
        video_codec: Some("h264".into()),
        audio_codec: Some("aac".into()),
        bitrate_bps: Some(4_000_000),
        is_video: true,
        ..Default::default()
    };
    let decision = negotiate(&mp4_h264_aac_profile(), &source);
    match decision {
        Decision::VideoRemux {
            target_container,
            target_audio_codec,
        } => {
            assert_eq!(target_container, "mp4");
            // Audio codec matches → copies, no transcode target.
            assert_eq!(target_audio_codec, None);
        }
        other => panic!("expected VideoRemux, got {other:?}"),
    }
}

#[::core::prelude::v1::test]
fn mkv_h264_ac3_against_mp4_profile_remuxes_with_audio_aac_target() {
    let source = SourceMedia {
        container: "matroska".into(),
        video_codec: Some("h264".into()),
        audio_codec: Some("ac3".into()),
        bitrate_bps: Some(4_000_000),
        is_video: true,
        ..Default::default()
    };
    let decision = negotiate(&mp4_h264_aac_profile(), &source);
    match decision {
        Decision::VideoRemux {
            target_container,
            target_audio_codec,
        } => {
            assert_eq!(target_container, "mp4");
            assert_eq!(target_audio_codec.as_deref(), Some("aac"));
        }
        other => panic!("expected VideoRemux, got {other:?}"),
    }
}

#[::core::prelude::v1::test]
fn mkv_vp9_against_mp4_h264_profile_does_not_remux() {
    // Video codec mismatch falls through to Transcode — remux can't
    // change the video bitstream.
    let source = SourceMedia {
        container: "matroska".into(),
        video_codec: Some("vp9".into()),
        audio_codec: Some("aac".into()),
        bitrate_bps: Some(4_000_000),
        is_video: true,
        ..Default::default()
    };
    let decision = negotiate(&mp4_h264_aac_profile(), &source);
    assert!(
        matches!(decision, Decision::Transcode { .. }),
        "{decision:?}"
    );
}

#[::core::prelude::v1::test]
fn matching_container_still_takes_direct_play_or_audio_remux_path() {
    // MP4 source matching mp4 profile → DirectPlay; container check
    // never relaxes.
    let source = SourceMedia {
        container: "mp4".into(),
        video_codec: Some("h264".into()),
        audio_codec: Some("aac".into()),
        bitrate_bps: Some(4_000_000),
        is_video: true,
        ..Default::default()
    };
    let decision = negotiate(&mp4_h264_aac_profile(), &source);
    assert!(matches!(decision, Decision::DirectPlay), "{decision:?}");
}

#[actix_web::test]
async fn playback_info_for_remux_emits_transcoding_url_and_target_container() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
            id: 11,
            path: "/nonexistent.mkv".into(),
            title: "MKV Movie".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("matroska".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("ac3".into()),
                ..Default::default()
            },
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
        .uri("/Items/11/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload(
            r#"{"DeviceProfile":{
              "DirectPlayProfiles":[{
                "Container":"mp4","Type":"Video",
                "VideoCodec":"h264","AudioCodec":"aac"
              }]
            }}"#,
        )
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["MediaSources"][0]["SupportsDirectStream"]
            .as_bool()
            .unwrap_or(false),
        "{v}"
    );
    assert_eq!(
        v["MediaSources"][0]["Container"].as_str(),
        Some("mp4"),
        "{v}"
    );
    assert!(v["MediaSources"][0]["TranscodingUrl"].is_string(), "{v}");
    assert_eq!(
        v["MediaSources"][0]["TranscodingSubProtocol"].as_str(),
        Some("hls"),
        "{v}"
    );
}

// A legacy mpeg4/AVI source can't direct-play or remux → it must transcode.
// A client whose HLS transcoding profile requests container "mp4"
// (jellyfin-web's Firefox profile) must STILL get a TranscodingUrl — pharos
// serves mpegts H.264 HLS regardless. Gating the URL on
// target_container=="ts" returned SupportsTranscoding:true with a null URL and
// broke jellyfin-web with "error processing the request".
#[actix_web::test]
async fn playback_info_video_transcode_emits_url_for_non_ts_container_profile() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
            id: 12,
            path: "/legacy.avi".into(),
            title: "AVI Movie".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(624),
                height: Some(352),
                bitrate_bps: Some(1_000_000),
                container: Some("avi".into()),
                video_codec: Some("mpeg4".into()),
                audio_codec: Some("mp3".into()),
                ..Default::default()
            },
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
        .uri("/Items/12/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload(
            r#"{"DeviceProfile":{
              "DirectPlayProfiles":[],
              "TranscodingProfiles":[{
                "Container":"mp4","Type":"Video","Protocol":"hls",
                "VideoCodec":"h264","AudioCodec":"aac"
              }]
            }}"#,
        )
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ms = &v["MediaSources"][0];
    assert_eq!(ms["SupportsTranscoding"].as_bool(), Some(true), "{v}");
    let url = ms["TranscodingUrl"].as_str();
    assert!(
        url.is_some_and(|u| u.contains("master.m3u8")),
        "video transcode must emit an HLS master URL, got {v}"
    );
    // hls.js fetches the master playlist with NO auth header, so the token
    // MUST ride in the URL as `api_key` — otherwise the first manifest load
    // 401s and playback dies with a fatal manifestLoadError.
    assert!(
        url.is_some_and(|u| u.contains(&format!("api_key={}", token.0.expose()))),
        "TranscodingUrl must embed the caller's token as api_key, got {v}"
    );
    assert_eq!(ms["TranscodingSubProtocol"].as_str(), Some("hls"), "{v}");
}
