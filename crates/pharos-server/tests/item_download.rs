#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T025) — `GET /Items/{id}/Download`, the second hard blocker.
//!
//! This is the URL all three jellyfin-web readers construct via
//! `getItemDownloadUrl`, and the route did not exist. Two properties are easy to
//! get wrong and both are asserted:
//!
//! 1. **Query auth alone must work.** The client builds
//!    `Items/{id}/Download?api_key=…` and sends **no** `Authorization` header.
//!    A handler that required the header would 401 every book with the client
//!    behaving exactly as designed.
//! 2. **A HEAD must advertise the real length.** actix derives `Content-Length`
//!    from the body's declared `BodySize` and discards a hand-set header, so a
//!    handler answering with `.finish()` says 0 — B166 on the image path, B101
//!    on the video path, now V113.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;

/// Recognisable, incompressible-enough payload so a truncated or offset body is
/// obvious rather than coincidentally equal.
fn payload() -> Vec<u8> {
    (0..4096u32).map(|i| (i % 251) as u8).collect()
}

async fn seed(dir: &std::path::Path) -> (web::Data<AppState>, String, std::path::PathBuf) {
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

    let path = dir.join("Dune.epub");
    std::fs::write(&path, payload()).unwrap();

    stores
        .put(MediaItem {
            id: 42,
            path: path.clone(),
            title: "Dune".into(),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format: BookFormat::Epub,
                ..Default::default()
            }),
            probe: MediaProbe::default(),
            series: None,
            created_at: Some(1_700_000_000),
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        })
        .await
        .unwrap();

    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string(), path)
}

fn app(
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
async fn query_auth_alone_serves_the_exact_bytes() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path()).await;
    let app = test::init_service(app(state)).await;

    // NO Authorization header — exactly how the client calls it.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/42/Download?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "query auth must be accepted; the reader sends no Authorization header"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let cd = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let ar = resp
        .headers()
        .get("accept-ranges")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(ct, "application/epub+zip");
    assert_eq!(cd, "attachment; filename=\"Dune.epub\"");
    assert_eq!(ar, "bytes", "a reader needs range support to seek");

    let body = test::read_body(resp).await;
    assert_eq!(
        body.as_ref(),
        payload().as_slice(),
        "the body must be the file, byte for byte"
    );
}

#[actix_web::test]
async fn a_head_advertises_the_length_the_get_would_send() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path()).await;
    let app = test::init_service(app(state)).await;

    let head = test::call_service(
        &app,
        test::TestRequest::with_uri(&format!("/Items/42/Download?api_key={token}"))
            .method(actix_web::http::Method::HEAD)
            .to_request(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);

    // The header itself is stamped by the h1 encoder at wire time, from the
    // body's declared size — which is exactly the value B166 got wrong. So
    // assert the MECHANISM, not a header the in-process harness never
    // materialises. This mirrors how images.rs proves the same property.
    use actix_web::body::MessageBody;
    assert_eq!(
        head.into_body().size(),
        actix_web::body::BodySize::Sized(payload().len() as u64),
        "a HEAD must declare the length the GET would send (V113/B166)"
    );
}

#[actix_web::test]
async fn a_range_request_is_honoured() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path()).await;
    let app = test::init_service(app(state)).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/42/Download?api_key={token}"))
            .insert_header(("Range", "bytes=0-99"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let cr = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(cr, format!("bytes 0-99/{}", payload().len()));

    let body = test::read_body(resp).await;
    assert_eq!(body.len(), 100);
    assert_eq!(
        body.as_ref(),
        &payload()[..100],
        "a range must return the requested slice, not the head of the file by luck"
    );
}

#[actix_web::test]
async fn an_unknown_id_is_404_and_a_missing_token_is_401() {
    let td = TempDir::new().unwrap();
    let (state, token, path) = seed(td.path()).await;
    let app = test::init_service(app(state)).await;

    // Unknown id.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/999/Download?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // No token at all.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Items/42/Download")
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an unauthenticated caller must not receive file bytes (V9)"
    );

    // The row exists but the file is gone — a 404, distinguished in the log from
    // an unknown id. This is the shape a dead NFS mount takes.
    std::fs::remove_file(&path).unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/42/Download?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn download_is_not_restricted_to_books() {
    // Real Jellyfin's /Download serves any item and CanDownload is advertised
    // generally. Restricting it to Type=Book would be a gratuitous divergence,
    // so a movie must download too.
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path()).await;
    let movie_path = td.path().join("Alien.mkv");
    std::fs::write(&movie_path, b"not a book").unwrap();
    state
        .stores
        .put(MediaItem {
            id: 43,
            path: movie_path,
            title: "Alien".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe::default(),
            series: None,
            created_at: Some(1_700_000_000),
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        })
        .await
        .unwrap();
    let app = test::init_service(app(state)).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/43/Download?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        ct, "application/octet-stream",
        "an unrecognised extension gets octet-stream rather than a guess"
    );
    assert_eq!(test::read_body(resp).await.as_ref(), b"not a book");
}
