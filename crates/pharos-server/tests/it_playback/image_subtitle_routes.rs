//! Spec 019 — the image-subtitle manifest + PNG routes refuse what they
//! cannot serve: no credential, a text track, a server with no extractor.
//! The served path is covered end to end by the Dioxus e2e on a real
//! `dvd_subtitle` file.

use actix_web::{http::StatusCode, test, web, App};
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

fn track(stream_index: u32, codec: &str) -> SubtitleTrack {
    SubtitleTrack {
        stream_index,
        language: Some("eng".into()),
        codec: Some(codec.into()),
        title: None,
        is_default: false,
        is_forced: false,
        is_hearing_impaired: false,
    }
}

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
    stores
        .put(MediaItem {
            locked: Default::default(),
            id: 9,
            path: "/nonexistent.mkv".into(),
            title: "m".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                subtitle_tracks: vec![track(2, "subrip"), track(3, "dvd_subtitle")],
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
            edition: None,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

async fn status_of(uri: &str) -> StatusCode {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let uri = uri.replace("{token}", &token);
    let req = test::TestRequest::get().uri(&uri).to_request();
    test::call_service(&app, req).await.status()
}

#[actix_web::test]
async fn image_subtitle_routes_need_a_credential() {
    assert_eq!(
        status_of("/videos/9/subtitles/3/images.json").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("/videos/9/subtitles/3/images/0.png").await,
        StatusCode::UNAUTHORIZED
    );
}

#[actix_web::test]
async fn a_text_track_has_no_images() {
    assert_eq!(
        status_of("/videos/9/subtitles/2/images.json?api_key={token}").await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[actix_web::test]
async fn an_unknown_stream_is_not_found() {
    assert_eq!(
        status_of("/videos/9/subtitles/7/images.json?api_key={token}").await,
        StatusCode::NOT_FOUND
    );
}

#[actix_web::test]
async fn a_server_without_an_extractor_serves_no_images() {
    assert_eq!(
        status_of("/videos/9/subtitles/3/images.json?api_key={token}").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of("/videos/9/subtitles/3/images/0.png?api_key={token}").await,
        StatusCode::NOT_FOUND
    );
}
