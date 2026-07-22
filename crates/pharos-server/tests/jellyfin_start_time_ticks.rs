#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P7 — `StartTimeTicks` on direct-play stream, gated by container cut-tolerance.
//!
//! A `StartTimeTicks` resume with no Range can only be served as a raw byte cut
//! of the source. That is decodable ONLY for a self-framing container
//! (MPEG-TS / ADTS / MP3), which resyncs from any packet. For a header-prefixed
//! mp4/mkv/webm the moov / EBML index lives at file start or EOF, so an interior
//! slice is headerless garbage — so pharos instead serves the WHOLE seekable
//! file and lets the client jump to the offset via its own container index.
//!
//! End-to-end via the actix in-process harness. Writes a synthetic file with
//! known bytes-per-second so we can assert byte offsets exactly.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{self},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;

use pharos_core::time::TICKS_PER_SECOND;

/// Seed an item whose container is chosen by `ext` (+ matching probe
/// `container`), with `bitrate × duration` bytes so the byte-offset math lands
/// on round numbers.
async fn seed(
    td: &std::path::Path,
    bitrate_bps: u64,
    duration_ms: u64,
    ext: &str,
    container: &str,
) -> (web::Data<AppState>, String, std::path::PathBuf) {
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

    let bytes_per_sec = bitrate_bps / 8;
    let total = bytes_per_sec * (duration_ms / 1000);
    let path = td.join(format!("fake.{ext}"));
    let payload: Vec<u8> = (0..total as usize).map(|i| (i % 256) as u8).collect();
    tokio::fs::write(&path, &payload).await.unwrap();

    stores
        .put(MediaItem {
            id: 7,
            path: path.clone(),
            title: "fake".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(duration_ms),
                bitrate_bps: Some(bitrate_bps),
                size_bytes: Some(total),
                container: Some(container.into()),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
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
async fn no_start_time_ticks_returns_full_body() {
    let td = TempDir::new().unwrap();
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000, "mp4", "mp4").await;
    let app = test::init_service(app(state)).await;

    let req = test::TestRequest::get()
        .uri(&format!("/Videos/7/stream?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let body = test::read_body(resp).await;
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(body.len(), on_disk.len());
}

// A self-framing MPEG-TS resyncs from any packet, so a StartTimeTicks byte cut
// is decodable: serve the 206 from the exact byte offset.
#[actix_web::test]
async fn resync_container_start_time_ticks_returns_206_byte_offset() {
    let td = TempDir::new().unwrap();
    // 1 Mbps × 10s = 1_250_000 bytes total. 1s in = byte 125_000.
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000, "ts", "mpegts").await;
    let app = test::init_service(app(state)).await;

    let start_ticks = TICKS_PER_SECOND;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks={start_ticks}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 206, "{}", resp.status());
    let content_range = resp
        .headers()
        .get(actix_web::http::header::CONTENT_RANGE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 125000-1249999/1250000");
    let body = test::read_body(resp).await;
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(body.len(), on_disk.len() - 125_000);
    assert_eq!(&body[..256], &on_disk[125_000..125_256]);
}

// A seek body over 16 MiB used to STRIP Content-Length and fall back to chunked
// framing (BodyStream → unsized), which some clients refuse to seek. It now uses
// a SizedStream, so the body declares its exact length. The wire Content-Length
// of a response IS the body's `BodySize` (what the encoder writes), so assert on
// that directly — a header check can't see actix's encoder-computed length.
#[actix_web::test]
async fn large_resync_seek_declares_sized_body_not_chunked() {
    use actix_web::body::{BodySize, MessageBody};
    let td = TempDir::new().unwrap();
    // 20 Mbps × 10s = 25_000_000 bytes (> the 16 MiB buffer threshold). Seek 1s
    // in → offset 2_500_000, remaining 22_500_000.
    let (state, token, _path) = seed(td.path(), 20_000_000, 10_000, "ts", "mpegts").await;
    let app = test::init_service(app(state)).await;

    let start_ticks = TICKS_PER_SECOND;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/Videos/7/stream?api_key={token}&StartTimeTicks={start_ticks}"
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 206, "{}", resp.status());
    assert_eq!(
        resp.into_body().size(),
        BodySize::Sized(22_500_000),
        "a large seek 206 must declare its length (SizedStream), not fall back to chunked"
    );
}

#[actix_web::test]
async fn resync_container_start_time_ticks_past_eof_returns_416() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path(), 1_000_000, 10_000, "ts", "mpegts").await;
    let app = test::init_service(app(state)).await;

    let past_eof = 100 * TICKS_PER_SECOND; // 100s into a 10s source
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks={past_eof}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 416, "{}", resp.status());
}

// A header-prefixed mp4 CANNOT be interior-cut (moov at file start/EOF): a raw
// slice from byte 125000 omits the header and is undecodable. The old code
// shipped exactly that as a 206. pharos now falls through to the whole seekable
// file so the client self-seeks — the response MUST contain byte 0 (the header)
// and span the whole file, never a headerless interior slice.
#[actix_web::test]
async fn header_prefixed_start_time_ticks_serves_whole_seekable_file() {
    let td = TempDir::new().unwrap();
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000, "mp4", "mp4").await;
    let app = test::init_service(app(state)).await;

    let start_ticks = TICKS_PER_SECOND;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks={start_ticks}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    // Whole-file open: 200, Accept-Ranges advertised so the client can seek.
    assert_eq!(
        resp.status().as_u16(),
        200,
        "an mp4 StartTimeTicks resume must serve the whole seekable file, not a headerless 206"
    );
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes"),
        "the whole-file response must advertise range support for client self-seek"
    );
    let body = test::read_body(resp).await;
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(body.len(), on_disk.len(), "must be the whole file");
    assert_eq!(
        &body[..256],
        &on_disk[..256],
        "must include byte 0 (the header)"
    );
}

// Past-EOF StartTimeTicks on a header-prefixed source is not a 416 either — the
// offset is simply ignored and the whole file is served (the client seeks).
#[actix_web::test]
async fn header_prefixed_past_eof_start_time_ticks_serves_whole_file() {
    let td = TempDir::new().unwrap();
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000, "mp4", "mp4").await;
    let app = test::init_service(app(state)).await;

    let past_eof = 100 * TICKS_PER_SECOND;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks={past_eof}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200, "{}", resp.status());
    let body = test::read_body(resp).await;
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(body.len(), on_disk.len());
}

#[actix_web::test]
async fn range_header_wins_over_start_time_ticks() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path(), 1_000_000, 10_000, "mp4", "mp4").await;
    let app = test::init_service(app(state)).await;

    // Range covers bytes 100-199; StartTimeTicks would ask for byte 125_000 if
    // applied. NamedFile must pick Range (Jellyfin parity).
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks=10000000"
        ))
        .insert_header(("Range", "bytes=100-199"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 206, "{}", resp.status());
    let content_range = resp
        .headers()
        .get(actix_web::http::header::CONTENT_RANGE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 100-199/1250000");
}
