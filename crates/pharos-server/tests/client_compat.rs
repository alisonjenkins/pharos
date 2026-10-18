#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T29 phase 1A — Jellyfin client-compat smoke.
//!
//! Spawns pharos-server via `actix_test::start` on an ephemeral port,
//! drives `pharos-jellyfin-test-client` through:
//!   1. GET /System/Info/Public         (unauth)
//!   2. POST /Users/AuthenticateByName  (Emby/MediaBrowser headers + token receipt)
//!   3. GET /Users/Me                   (token auth)
//!   4. GET /Items                      (browse)
//!   5. GET /Items/{id}                 (detail)
//!   6. GET /Library/VirtualFolders     (collections)
//!   7. HEAD /Videos/{id}/stream        (direct play range probe)
//!   8. GET /Sessions                   (active session list)
//!
//! Strict deserialization on every response — a missing or wrong-cased
//! field surfaces as `ClientError::Parse`. That's the bit
//! `tests/jellyfin_api.rs` could not assert via the in-process actix
//! harness.

use actix_test::TestServer;
use actix_web::{web, App};
use pharos_cache::hls_cache::{SegmentIdentity, SourceGen};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SeriesInfo, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore, Video3DFormat,
};
use pharos_jellyfin_test_client::{DeviceInfo, JellyfinClient};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use pharos_sync::GroupRegistry;
use pharos_transcode::{AudioDelivery, SegmentContainer, SegmentOpts, SegmentVideo, StereoOutput};

async fn boot_server() -> (TestServer, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();

    for (i, kind) in [
        MediaKind::Movie,
        MediaKind::Episode,
        MediaKind::Audio,
        MediaKind::Movie,
    ]
    .iter()
    .enumerate()
    {
        stores
            .put(MediaItem {
                id: (100 + i) as u64,
                path: format!("/m/{i}.x").into(),
                title: format!("title-{i}"),
                kind: *kind,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "pharos-compat".into()));
    let member_sinks = pharos_sync::MemberSinks::new();
    let registry = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(member_sinks.clone()),
    )));
    let member_sinks = web::Data::new(member_sinks);
    let server = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .app_data(member_sinks.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let url = server.url("").trim_end_matches('/').to_string();
    (server, url)
}

#[actix_web::test]
async fn full_jellyfin_client_flow() {
    let (_server, base) = boot_server().await;
    let mut client = JellyfinClient::new(base, DeviceInfo::default());

    // 1. Public system info — no auth required.
    let info = client.system_info_public().await.unwrap();
    assert_eq!(info.product_name, "Jellyfin Server");
    assert_eq!(info.server_name, "pharos-compat");
    assert!(!info.id.is_empty());
    assert!(!info.version.is_empty());

    // 2. Authenticate. Client stashes the token internally.
    let auth = client.authenticate_by_name("ali", "hunter2").await.unwrap();
    assert!(!auth.access_token.is_empty());
    assert_eq!(auth.user.name, "ali");
    assert!(auth.user.policy.is_administrator);
    assert!(auth.user.policy.enable_media_playback);
    assert_eq!(auth.server_id, info.id);
    assert!(client.token().is_some());

    // 3. /Users/Me with token.
    let me = client.users_me().await.unwrap();
    assert_eq!(me.id, auth.user.id);
    assert_eq!(me.name, "ali");

    // 4. Browse — pagination + total count are required fields.
    let items = client.items().await.unwrap();
    assert_eq!(items.total_record_count, 4);
    assert_eq!(items.items.len(), 4);
    let first = &items.items[0];
    assert!(!first.id.is_empty());
    assert!(!first.name.is_empty());
    // Type one of the strings clients pattern-match on.
    assert!(matches!(first.kind.as_str(), "Movie" | "Episode" | "Audio"));

    // 5. Detail by id.
    let one = client.item(&first.id).await.unwrap();
    assert_eq!(one.id, first.id);
    assert_eq!(one.server_id, info.id);

    // 6. Collections summary — clients pull this on startup.
    let folders = client.library_virtual_folders().await.unwrap();
    assert!(!folders.is_empty());
    assert_eq!(folders[0].name, "All Media");

    // 7. Direct-play HEAD. Path on disk does not exist — server should
    //    surface that as 404 rather than 500 or hang.
    let status = client.videos_stream_head(&first.id).await.unwrap();
    assert!(
        status.is_client_error() || status.is_success(),
        "unexpected stream status: {status}"
    );

    // 8. Sessions snapshot. Empty until a POST /Sessions/Playing fires
    //    — what matters here is that the endpoint deserializes to a list.
    let sessions = client.sessions().await.unwrap();
    assert!(sessions.is_empty());

    // 9-15: extended endpoints added since this test was written
    // (Genres / Artists / Albums / Suggestions / NextUp / SyncPlay
    // / Sessions remote). Drive them via awc with the same bearer
    // token; assert (status < 500, well-formed JSON envelope where
    // applicable). Catches mid-roll wire-shape regressions.
    let token = client.token().expect("authenticated").to_string();
    let base = client.base_url().to_string();
    let c = awc::Client::default();
    for path in [
        "/Genres",
        "/Artists",
        "/Albums",
        "/Studios",
        "/Search/Suggestions",
        &format!("/Users/{}/Suggestions", auth.user.id),
        "/Shows/NextUp?Limit=5",
        "/SyncPlay/List",
        &format!("/Items/{}/Similar", first.id),
    ] {
        let mut resp = c
            .get(&format!("{base}{path}"))
            .insert_header(("X-Emby-Token", token.as_str()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{path} → {}", resp.status());
        let body = resp.body().await.unwrap();
        let _v: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{path}: bad json {e}"));
    }
    // PlaybackInfo with a realistic device profile must return
    // SupportsDirectPlay alongside Container/MediaStreams.
    let mut pi = c
        .post(&format!("{base}/Items/{}/PlaybackInfo", first.id))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(
            r#"{"DeviceProfile":{
              "DirectPlayProfiles":[{"Container":"webm","Type":"Video"}]
            }}"#,
        )
        .await
        .unwrap();
    assert!(pi.status().is_success(), "PlaybackInfo: {}", pi.status());
    let body = pi.body().await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["MediaSources"][0]["Container"].is_string());
    assert!(v["PlaySessionId"].is_string());
    // Sessions remote-control: Pause command should 204 even with
    // no live session matching the id (the bus delivers; nobody
    // listens).
    let pause = c
        .post(&format!("{base}/Sessions/no-such/Playing/Pause"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body("{}")
        .await
        .unwrap();
    assert_eq!(pause.status().as_u16(), 204);
}

#[actix_web::test]
async fn wrong_credentials_return_401() {
    let (_server, base) = boot_server().await;
    let mut client = JellyfinClient::new(base, DeviceInfo::default());
    let err = client
        .authenticate_by_name("ali", "wrong-password")
        .await
        .expect_err("must reject wrong password");
    match err {
        pharos_jellyfin_test_client::ClientError::Status { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401 status error, got {other:?}"),
    }
}

#[actix_web::test]
async fn unauthenticated_calls_return_401() {
    let (_server, base) = boot_server().await;
    let client = JellyfinClient::new(base, DeviceInfo::default());
    let err = client.users_me().await.expect_err("must require auth");
    match err {
        pharos_jellyfin_test_client::ClientError::Status { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
}

async fn boot_server_3d() -> (TestServer, String, String, u64, u64) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let movie_id = 201_u64;
    let episode_id = 202_u64;

    stores
        .put(MediaItem {
            id: movie_id,
            path: "/m/201.mp4".into(),
            title: "3D SBS Movie".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                audio_channels: Some(2),
                video_3d_format: Some(Video3DFormat::HalfSideBySide),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    stores
        .put(MediaItem {
            id: episode_id,
            path: "/m/202.mp4".into(),
            title: "3D TAB Episode".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "3D Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: None,
                series_year: None,
            }),
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                audio_channels: Some(2),
                video_3d_format: Some(Video3DFormat::HalfTopAndBottom),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    let token = stores.issue(uid, "compat-3d").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "pharos-compat-3d".into()));
    let member_sinks = pharos_sync::MemberSinks::new();
    let registry = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(member_sinks.clone()),
    )));
    let member_sinks = web::Data::new(member_sinks);
    let server = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .app_data(member_sinks.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let url = server.url("").trim_end_matches('/').to_string();
    (
        server,
        url,
        token.0.expose().to_string(),
        movie_id,
        episode_id,
    )
}

#[actix_web::test]
async fn three_d_playback_negotiation_and_stereo_mode_isolation() {
    let (_server, base, token, movie_id, episode_id) = boot_server_3d().await;
    let c = awc::Client::default();

    // Default profile allowing DirectPlay of h264/aac in mp4
    let default_profile = r#"{"DeviceProfile":{
        "DirectPlayProfiles":[
            {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
        ],
        "TranscodingProfiles":[
            {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
        ]
    }}"#;

    // 1. POST /Items/{id}/PlaybackInfo across default client profiles
    // Movie (HalfSideBySide)
    let mut pi_movie = c
        .post(&format!("{base}/Items/{movie_id}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(default_profile)
        .await
        .unwrap();
    assert!(pi_movie.status().is_success());
    let v_movie: serde_json::Value =
        serde_json::from_slice(&pi_movie.body().await.unwrap()).unwrap();
    let src_movie = &v_movie["MediaSources"][0];
    assert_eq!(src_movie["SupportsDirectPlay"], true);
    assert_eq!(src_movie["Video3DFormat"], "HalfSideBySide");
    let v_stream_movie = src_movie["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["Type"] == "Video")
        .expect("video stream on movie");
    assert_eq!(v_stream_movie["Video3DFormat"], "HalfSideBySide");

    // Episode (HalfTopAndBottom)
    let mut pi_ep = c
        .post(&format!("{base}/Items/{episode_id}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(default_profile)
        .await
        .unwrap();
    assert!(pi_ep.status().is_success());
    let v_ep: serde_json::Value = serde_json::from_slice(&pi_ep.body().await.unwrap()).unwrap();
    let src_ep = &v_ep["MediaSources"][0];
    assert_eq!(src_ep["SupportsDirectPlay"], true);
    assert_eq!(src_ep["Video3DFormat"], "HalfTopAndBottom");
    let v_stream_ep = src_ep["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["Type"] == "Video")
        .expect("video stream on episode");
    assert_eq!(v_stream_ep["Video3DFormat"], "HalfTopAndBottom");

    // 2. POST /Items/{id}/PlaybackInfo with DeviceProfile requiring Is3D=false
    let profile_reject_3d = r#"{"DeviceProfile":{
        "DirectPlayProfiles":[
            {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
        ],
        "CodecProfiles":[
            {
                "Type":"Video",
                "Codec":"h264",
                "Conditions":[
                    {"Condition":"Equals","Property":"Is3D","Value":"false","IsRequired":true}
                ]
            }
        ],
        "TranscodingProfiles":[
            {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
        ]
    }}"#;

    let mut pi_rejected = c
        .post(&format!("{base}/Items/{movie_id}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(profile_reject_3d)
        .await
        .unwrap();
    assert!(pi_rejected.status().is_success());
    let v_rejected: serde_json::Value =
        serde_json::from_slice(&pi_rejected.body().await.unwrap()).unwrap();
    let src_rej = &v_rejected["MediaSources"][0];
    assert_eq!(src_rej["SupportsDirectPlay"], false);
    assert_eq!(src_rej["SupportsDirectStream"], false);
    assert_eq!(src_rej["SupportsTranscoding"], true);
    let transcode_url = src_rej["TranscodingUrl"]
        .as_str()
        .expect("TranscodingUrl present");
    assert!(
        transcode_url.contains("StereoMode=mono"),
        "TranscodingUrl must contain StereoMode=mono: {transcode_url}"
    );

    // 3. POST /Items/{id}/PlaybackInfo?StereoMode=mono with unrestricted profile
    let mut pi_explicit_mono = c
        .post(&format!(
            "{base}/Items/{movie_id}/PlaybackInfo?StereoMode=mono"
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(default_profile)
        .await
        .unwrap();
    assert!(pi_explicit_mono.status().is_success());
    let v_mono: serde_json::Value =
        serde_json::from_slice(&pi_explicit_mono.body().await.unwrap()).unwrap();
    let src_mono = &v_mono["MediaSources"][0];
    assert_eq!(src_mono["SupportsDirectPlay"], false);
    assert_eq!(src_mono["SupportsDirectStream"], false);
    assert_eq!(src_mono["SupportsTranscoding"], true);
    let transcode_url_mono = src_mono["TranscodingUrl"]
        .as_str()
        .expect("TranscodingUrl present");
    assert!(
        transcode_url_mono.contains("StereoMode=mono"),
        "TranscodingUrl must contain StereoMode=mono: {transcode_url_mono}"
    );

    // 4. GET /Videos/{id}/master.m3u8?StereoMode=mono and GET /Videos/{id}/main.m3u8?StereoMode=mono
    let mut master_resp = c
        .get(&format!(
            "{base}/Videos/{movie_id}/master.m3u8?StereoMode=mono"
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .send()
        .await
        .unwrap();
    assert!(master_resp.status().is_success());
    let master_body = String::from_utf8(master_resp.body().await.unwrap().to_vec()).unwrap();
    assert!(
        master_body.contains("StereoMode=mono"),
        "master playlist must forward StereoMode=mono: {master_body}"
    );

    let mut main_resp = c
        .get(&format!(
            "{base}/Videos/{movie_id}/main.m3u8?StereoMode=mono"
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .send()
        .await
        .unwrap();
    assert!(main_resp.status().is_success());
    let main_body = String::from_utf8(main_resp.body().await.unwrap().to_vec()).unwrap();
    assert!(
        main_body.contains("StereoMode=mono"),
        "main playlist must forward StereoMode=mono: {main_body}"
    );

    // 5. SegmentIdentity isolation (Preserve vs Mono2D)
    let make_opts = |stereo: StereoOutput| SegmentOpts {
        source_video_codec: None,
        source_video_kind: Default::default(),
        container: SegmentContainer::Mpegts,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(4_000_000),
        window: pharos_core::SegmentWindow::for_segment(0, None, Some(60.0)),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        video_3d_format: Some(Video3DFormat::HalfSideBySide),
        stereo_output: stereo,
    };

    let key_preserve = SegmentIdentity::new(
        movie_id,
        0,
        Some(1),
        None,
        &make_opts(StereoOutput::Preserve),
        SourceGen::LOCAL,
    );
    let key_mono = SegmentIdentity::new(
        movie_id,
        0,
        Some(1),
        None,
        &make_opts(StereoOutput::Mono2D),
        SourceGen::LOCAL,
    );

    assert_ne!(
        key_preserve.etag(),
        key_mono.etag(),
        "SegmentIdentity with Mono2D vs Preserve must produce distinct ETags"
    );
    assert!(
        format!("{key_mono:?}").contains("Mono2D"),
        "Mono2D key debug representation must contain Mono2D"
    );
    assert!(
        !format!("{key_preserve:?}").contains("Mono2D"),
        "Preserve key debug representation must not contain Mono2D"
    );
}
