#![allow(clippy::unwrap_used, clippy::expect_used)]
//! PlaybackInfo body switches `EnableDirectPlay` / `EnableDirectStream` /
//! `EnableTranscoding`: jellyfin-web sends `false` to force a transcode when
//! direct play failed. Absent fields keep the negotiated behaviour.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{self, device_profile::Decision},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const PROFILE: &str = r#""DeviceProfile":{
  "DirectPlayProfiles":[{"Type":"Video","Container":"mp4","VideoCodec":"h264","AudioCodec":"aac"}],
  "TranscodingProfiles":[{"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}]
}"#;

/// Item 1: mp4 h264/aac (direct plays). Item 2: avi mpeg4/mp3 (must transcode).
async fn setup() -> (web::Data<AppState>, String) {
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
    for (id, path, container, vcodec, acodec) in [
        (1u64, "/a.mp4", "mp4", "h264", "aac"),
        (2u64, "/b.avi", "avi", "mpeg4", "mp3"),
        (3u64, "/c.mp4", "mp4", "h264", "mp3"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: path.into(),
                title: format!("Item{id}"),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    duration_ms: Some(60_000),
                    width: Some(1280),
                    height: Some(720),
                    bitrate_bps: Some(1_000_000),
                    container: Some(container.into()),
                    video_codec: Some(vcodec.into()),
                    audio_codec: Some(acodec.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

async fn playback_info(
    state: &web::Data<AppState>,
    token: &str,
    id: u64,
    switches: &str,
) -> serde_json::Value {
    playback_info_with(state, token, id, PROFILE, switches).await
}

async fn playback_info_with(
    state: &web::Data<AppState>,
    token: &str,
    id: u64,
    profile: &str,
    switches: &str,
) -> serde_json::Value {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{id}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", token))
        .insert_header(("content-type", "application/json"))
        .set_payload(format!("{{{profile}{switches}}}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    serde_json::from_slice(&body).unwrap()
}

fn flags(v: &serde_json::Value) -> (bool, bool, bool, bool) {
    let ms = &v["MediaSources"][0];
    (
        ms["SupportsDirectPlay"].as_bool().unwrap(),
        ms["SupportsDirectStream"].as_bool().unwrap(),
        ms["SupportsTranscoding"].as_bool().unwrap(),
        ms["TranscodingUrl"].as_str().is_some(),
    )
}

#[actix_web::test]
async fn absent_switches_keep_the_negotiated_direct_play() {
    let (state, token) = setup().await;
    let v = playback_info(&state, &token, 1, "").await;
    assert_eq!(flags(&v), (true, true, true, false), "{v}");
    assert!(v.get("ErrorCode").is_none(), "{v}");
}

#[actix_web::test]
async fn disabling_direct_play_and_stream_forces_a_transcode_url() {
    let (state, token) = setup().await;
    let v = playback_info(
        &state,
        &token,
        1,
        r#","EnableDirectPlay":false,"EnableDirectStream":false"#,
    )
    .await;
    assert_eq!(flags(&v), (false, false, true, true), "{v}");
}

#[actix_web::test]
async fn disabling_only_direct_play_still_builds_a_transcoding_url() {
    let (state, token) = setup().await;
    let v = playback_info(&state, &token, 1, r#","EnableDirectPlay":false"#).await;
    assert_eq!(flags(&v), (false, true, true, true), "{v}");
}

#[actix_web::test]
async fn forbidding_video_copy_clears_direct_stream() {
    let (state, token) = setup().await;
    let v = playback_info(
        &state,
        &token,
        1,
        r#","EnableDirectPlay":false,"AllowVideoStreamCopy":false"#,
    )
    .await;
    assert_eq!(flags(&v), (false, false, true, true), "{v}");
}

/// mp4 h264/mp3 against an mp4 h264/aac profile is an AudioRemux: the video
/// would copy. With video copy forbidden the URL must be a full transcode.
#[actix_web::test]
async fn forbidding_video_copy_turns_an_audio_remux_into_a_transcode() {
    let (state, token) = setup().await;
    let registered = |v: serde_json::Value| {
        let state = state.clone();
        async move {
            let id = v["PlaySessionId"].as_str().unwrap().to_string();
            state
                .transcode_sessions
                .get(&id)
                .await
                .unwrap()
                .unwrap()
                .decision
        }
    };
    let copy_allowed = playback_info(
        &state,
        &token,
        3,
        r#","EnableDirectPlay":false,"EnableDirectStream":false"#,
    )
    .await;
    assert!(
        matches!(registered(copy_allowed).await, Decision::AudioRemux { .. }),
        "control: the source is an AudioRemux"
    );
    let copy_forbidden = playback_info(
        &state,
        &token,
        3,
        r#","EnableDirectPlay":false,"EnableDirectStream":false,"AllowVideoStreamCopy":false"#,
    )
    .await;
    assert!(matches!(
        registered(copy_forbidden).await,
        Decision::Transcode { .. }
    ));
}

/// A client whose only transcoding target is mp4/hevc must not be handed the
/// hardcoded h264-in-ts target when it forces a transcode.
#[actix_web::test]
async fn forced_transcode_uses_the_clients_own_transcoding_profile() {
    let (state, token) = setup().await;
    let hevc = r#""DeviceProfile":{
      "DirectPlayProfiles":[{"Type":"Video","Container":"mp4","VideoCodec":"h264","AudioCodec":"aac"}],
      "TranscodingProfiles":[{"Container":"mp4","Type":"Video","Protocol":"hls","VideoCodec":"hevc","AudioCodec":"aac"}]
    }"#;
    let v = playback_info_with(&state, &token, 1, hevc, r#","EnableDirectPlay":false"#).await;
    let id = v["PlaySessionId"].as_str().unwrap();
    let decision = state
        .transcode_sessions
        .get(id)
        .await
        .unwrap()
        .unwrap()
        .decision;
    match decision {
        Decision::Transcode {
            target_container, ..
        } => assert_eq!(target_container, "mp4", "hardcoded ts target leaked"),
        other => panic!("expected a transcode, got {other:?}"),
    }
}

#[actix_web::test]
async fn disabling_transcoding_drops_the_url_and_reports_no_compatible_stream() {
    let (state, token) = setup().await;
    let v = playback_info(&state, &token, 2, r#","EnableTranscoding":false"#).await;
    assert_eq!(flags(&v), (false, false, false, false), "{v}");
    assert_eq!(v["ErrorCode"].as_str(), Some("NoCompatibleStream"), "{v}");
}

#[actix_web::test]
async fn every_path_disabled_yields_no_playable_source() {
    let (state, token) = setup().await;
    let v = playback_info(
        &state,
        &token,
        1,
        r#","EnableDirectPlay":false,"EnableDirectStream":false,"EnableTranscoding":false"#,
    )
    .await;
    assert_eq!(flags(&v), (false, false, false, false), "{v}");
    assert_eq!(v["ErrorCode"].as_str(), Some("NoCompatibleStream"), "{v}");
}

#[actix_web::test]
async fn direct_play_stays_playable_when_only_transcoding_is_disabled() {
    let (state, token) = setup().await;
    let v = playback_info(&state, &token, 1, r#","EnableTranscoding":false"#).await;
    assert_eq!(flags(&v), (true, true, false, false), "{v}");
    assert!(v.get("ErrorCode").is_none(), "{v}");
}
