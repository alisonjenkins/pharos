#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
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

// Repro probe: a browser SEEK sends an OPEN-ENDED mid-file range (`bytes=N-`,
// not the closed `bytes=4-9` the existing test covers) and echoes the opening
// response's validator back as `If-Range`. If either shape yields a 200/full
// body, the browser can only seek within what it has already buffered.
#[actix_web::test]
async fn seek_repro_open_ended_and_if_range() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;

    // 1) Opening probe: `bytes=0-` — capture the validators the browser stores.
    let open = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Videos/42/stream")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("Range", "bytes=0-"))
            .to_request(),
    )
    .await;
    let etag = open
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_string());
    let last_mod = open
        .headers()
        .get("last-modified")
        .map(|v| v.to_str().unwrap().to_string());
    let accept_ranges = open
        .headers()
        .get("accept-ranges")
        .map(|v| v.to_str().unwrap().to_string());
    eprintln!(
        "OPEN status={} etag={etag:?} last_mod={last_mod:?} accept_ranges={accept_ranges:?}",
        open.status()
    );

    // 2) Open-ended mid-file seek WITHOUT If-Range.
    let seek = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Videos/42/stream")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("Range", "bytes=20-"))
            .to_request(),
    )
    .await;
    let seek_status = seek.status();
    let seek_cr = seek
        .headers()
        .get("content-range")
        .map(|v| v.to_str().unwrap().to_string());
    let seek_body = test::read_body(seek).await;
    eprintln!(
        "SEEK(open-ended) status={seek_status} content-range={seek_cr:?} body_len={}",
        seek_body.len()
    );

    // 3) Open-ended mid-file seek WITH matching If-Range (what a browser sends).
    if let Some(validator) = etag.clone().or(last_mod.clone()) {
        let cond = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/Videos/42/stream")
                .insert_header(("X-Emby-Token", token.as_str()))
                .insert_header(("Range", "bytes=20-"))
                .insert_header(("If-Range", validator.as_str()))
                .to_request(),
        )
        .await;
        let cond_status = cond.status();
        let cond_body = test::read_body(cond).await;
        eprintln!(
            "SEEK(if-range={validator}) status={cond_status} body_len={}",
            cond_body.len()
        );
        assert_eq!(
            cond_status, 206,
            "matching If-Range seek must be 206 from offset, else browser can only seek within buffered"
        );
        assert_eq!(cond_body.as_ref(), &PAYLOAD[20..]);
    } else {
        eprintln!("NO VALIDATOR on opening response — browser has no If-Range basis");
    }

    assert_eq!(
        seek_status, 206,
        "open-ended mid-file seek must be 206 from offset"
    );
    assert_eq!(seek_body.as_ref(), &PAYLOAD[20..]);
    assert_eq!(
        accept_ranges.as_deref(),
        Some("bytes"),
        "Accept-Ranges: bytes is the seekability signal"
    );
}

/// Seed a VP9-in-Matroska item (a real Firefox DirectPlay shape) so the
/// `video/webm` relabel is exercised. The default `seed_with_file` uses an
/// empty probe, which never triggers the codec-conditional relabel.
async fn seed_vp9_mkv() -> (web::Data<AppState>, String, TempDir) {
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
            path,
            title: "movie".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                container: Some("matroska".into()),
                video_codec: Some("vp9".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string(), td)
}

// A WebM-legal Matroska (VP8/VP9/AV1) plays in Firefox as `video/webm`, but
// `mime_guess` maps `.mkv` to `video/x-matroska`, which Firefox rejects
// outright. The relabel used to live ONLY in deliver_stream's whole-file branch,
// so a Range seek and the HEAD seekability probe served the rejected type and
// regressed a stream that plain-opened fine. `seek::DeliveryMime` now computes
// the type once, so open / range-seek / HEAD must all agree on `video/webm`.
#[actix_web::test]
async fn webm_legal_mkv_serves_video_webm_on_open_seek_and_head() {
    let (state, token, _td) = seed_vp9_mkv().await;
    let app = test::init_service(build_app(state)).await;

    let content_type = |resp: &actix_web::dev::ServiceResponse| {
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };

    // 1) Plain open.
    let open = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Videos/42/stream")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(content_type(&open).as_deref(), Some("video/webm"), "open");

    // 2) Range seek (goes through the NamedFile + B94 branch).
    let seek = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Videos/42/stream")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("Range", "bytes=4-9"))
            .to_request(),
    )
    .await;
    assert_eq!(seek.status(), 206);
    assert_eq!(
        content_type(&seek).as_deref(),
        Some("video/webm"),
        "a range seek must not regress to video/x-matroska"
    );

    // 3) HEAD seekability probe.
    let head = test::call_service(
        &app,
        test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri("/Videos/42/stream.mkv")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(
        content_type(&head).as_deref(),
        Some("video/webm"),
        "the HEAD probe must advertise the same type the GET body carries"
    );
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

// B95 — Firefox HEAD-probes a progressive `<video>` source to learn whether the
// server supports byte ranges before it will treat the media as seekable. The
// DirectPlay URL jellyfin-web hands the browser carries an extension
// (`stream.mp4`), but HEAD was registered ONLY on the extension-less
// `/videos/{id}/stream`, so `HEAD /videos/{id}/stream.mp4` fell through to 404.
// Firefox read that as "no range support" and collapsed `seekable` to
// `buffered` — the user could only seek within already-downloaded bytes.
#[actix_web::test]
async fn head_alt_extension_route_advertises_ranges() {
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::default()
        .method(actix_web::http::Method::HEAD)
        .uri("/Videos/42/stream.mp4")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        200,
        "HEAD on the extensioned DirectPlay URL must succeed or Firefox marks it non-seekable"
    );
    assert_eq!(
        resp.headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok()),
        Some("bytes"),
        "HEAD must advertise Accept-Ranges: bytes"
    );
}

// B101 — the B95 HEAD handler advertised Accept-Ranges but reported
// `Content-Length: 0`. It built the response with `.finish()`, whose empty
// `()` body has `BodySize::Sized(0)`. actix's h1 encoder derives a HEAD
// response's Content-Length from the body's declared `BodySize` (the bytes are
// never transmitted) and drops any hand-inserted Content-Length header, so the
// real size the handler computed was replaced with 0. Firefox HEAD-probes a
// progressive `<video>` source to learn the resource length; for a
// non-faststart mp4 (moov atom at EOF, as most library files are) it must know
// the length to range-fetch the trailing `moov` seek index. A zero length reads
// as "nothing to seek", collapsing `seekable` to `buffered` — the user can only
// seek within already-downloaded bytes. The HEAD body must therefore declare
// the real file size.
#[actix_web::test]
async fn head_reports_real_content_length() {
    use actix_web::body::{BodySize, MessageBody};
    let (state, token, _td) = seed_with_file().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::default()
        .method(actix_web::http::Method::HEAD)
        .uri("/Videos/42/stream.mp4")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    // The wire Content-Length of a HEAD response is exactly the response body's
    // declared `BodySize` — that is what the encoder writes and what the browser
    // reads. Assert on it directly (a header-only check passes even on the buggy
    // `.finish()` path, because the clobber happens downstream in the encoder).
    assert_eq!(
        resp.into_body().size(),
        BodySize::Sized(PAYLOAD.len() as u64),
        "HEAD must report the real file size or Firefox marks a moov-at-EOF mp4 non-seekable"
    );
}
