#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::io::Write;
use tempfile::TempDir;

const PAYLOAD: &[u8] = b"FAKEMKV-payload-bytes-for-test-only";

async fn seed_with_file() -> (web::Data<AppState>, String, TempDir) {
    let td = TempDir::new().unwrap();
    let path = td.path().join("movie.mkv");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(PAYLOAD).unwrap();

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
    let token = stores.issue(uid, "test").await.unwrap();
    stores
        .put(MediaItem {
            id: 42,
            path: path.clone(),
            title: "movie".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string(), td)
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

#[actix_web::test]
async fn stream_returns_200_with_full_body_when_authed() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/42/stream")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = test::read_body(resp).await;
    assert_eq!(body.as_ref(), PAYLOAD);
}

#[actix_web::test]
async fn stream_requires_auth() {
    let (state, _t, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/42/stream")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn stream_accepts_api_key_query_param() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let uri = format!("/Videos/42/stream?api_key={token}");
    let req = test::TestRequest::get().uri(&uri).to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn stream_unknown_id_is_404() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/9999/stream")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn range_request_returns_206_partial() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/42/stream")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("Range", "bytes=4-9"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 206);
    let body = test::read_body(resp).await;
    assert_eq!(body.as_ref(), &PAYLOAD[4..=9]);
}

// B94 — Firefox's `<video>` opens playback with `Range: bytes=0-`. That range
// spans the whole file, and actix-files gates its 206 on `offset != 0 || length
// != total`, so it answers 200 (while still stamping Content-Range). Firefox
// reads the 200 as "server ignores ranges" and marks the media non-seekable.
// deliver_stream must promote any full-file Range response to 206.
#[actix_web::test]
async fn full_file_range_bytes_zero_dash_returns_206() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/42/stream")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("Range", "bytes=0-"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        206,
        "Firefox's opening `bytes=0-` probe must get 206 or seeking is disabled"
    );
    assert!(
        resp.headers().contains_key("content-range"),
        "206 must carry Content-Range"
    );
    let body = test::read_body(resp).await;
    assert_eq!(body.as_ref(), PAYLOAD);
}

#[actix_web::test]
async fn audio_universal_streams_file() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Audio/42/universal")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn stream_alt_extension_route_works() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Videos/42/stream.mkv")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}
