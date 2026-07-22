#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P8 — `EXT-X-MEDIA:TYPE=SUBTITLES` on master playlist + a per-track
//! subtitle playlist endpoint. HLS clients render a subtitle selector
//! instead of requiring burn-in.

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

async fn seed_with_subs(tracks: Vec<SubtitleTrack>) -> (web::Data<AppState>, String) {
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
            path: "/nonexistent.mkv".into(),
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

#[actix_web::test]
async fn master_playlist_does_not_advertise_in_manifest_subtitles() {
    let tracks = vec![
        SubtitleTrack {
            stream_index: 2,
            language: Some("eng".into()),
            codec: Some("subrip".into()),
            title: Some("English".into()),
            is_default: true,
            is_forced: false,
            is_hearing_impaired: false,
        },
        SubtitleTrack {
            stream_index: 3,
            language: Some("jpn".into()),
            codec: Some("subrip".into()),
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        },
    ];
    let (state, token) = seed_with_subs(tracks).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/Videos/9/master.m3u8?api_key={token}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    // Text subtitles are delivered as an External rendition via PlaybackInfo
    // (jellyfin-web renders them — SubtitlesOctopus / cue JSON). The master must
    // NOT also advertise an in-manifest subtitle rendition: hls.js would render
    // a second, unstyled copy on top of the External one ("subtitle twice").
    assert_eq!(
        s.matches("#EXT-X-MEDIA:TYPE=SUBTITLES").count(),
        0,
        "master must not carry in-manifest subtitle renditions:\n{s}"
    );
    assert!(
        !s.contains("SUBTITLES=\"subs\""),
        "no STREAM-INF should reference a subs group:\n{s}"
    );
}

#[actix_web::test]
async fn master_playlist_omits_subtitle_media_when_no_tracks() {
    let (state, token) = seed_with_subs(vec![]).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/Videos/9/master.m3u8?api_key={token}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(!s.contains("EXT-X-MEDIA"), "{s}");
    assert!(!s.contains("SUBTITLES=\"subs\""), "{s}");
}

#[actix_web::test]
async fn subtitle_playlist_returns_single_extinf_pointing_at_vtt() {
    let tracks = vec![SubtitleTrack {
        stream_index: 2,
        language: Some("eng".into()),
        codec: Some("subrip".into()),
        title: Some("English".into()),
        is_default: true,
        is_forced: false,
        is_hearing_impaired: false,
    }];
    let (state, token) = seed_with_subs(tracks).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/videos/9/subtitles/2.m3u8?api_key={token}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.starts_with("#EXTM3U"), "{s}");
    assert!(s.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{s}");
    assert_eq!(s.matches("#EXTINF").count(), 1, "{s}");
    assert!(s.contains("/videos/9/0/subtitles/2/stream.vtt"), "{s}");
    assert!(s.contains("#EXT-X-ENDLIST"), "{s}");
}
