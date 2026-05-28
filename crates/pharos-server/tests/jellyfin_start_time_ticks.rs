#![allow(clippy::unwrap_used, clippy::expect_used)]
//! P7 — `StartTimeTicks` on direct-play stream.
//!
//! End-to-end via the actix in-process harness. Writes a synthetic
//! file with known bytes-per-second so we can assert byte offsets
//! exactly.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{self},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use tempfile::TempDir;

const TICKS_PER_SECOND: u64 = 10_000_000;

async fn seed(
    td: &std::path::Path,
    bitrate_bps: u64,
    duration_ms: u64,
) -> (web::Data<AppState>, String, std::path::PathBuf) {
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

    // Synthesise a file of exactly bitrate × duration bytes so the
    // byte-offset math lands on round numbers.
    let bytes_per_sec = bitrate_bps / 8;
    let total = bytes_per_sec * (duration_ms / 1000);
    let path = td.join("fake.mp4");
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
                ..Default::default()
            },
            series: None,
            created_at: None,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string(), path)
}

#[actix_web::test]
async fn no_start_time_ticks_returns_full_body() {
    let td = TempDir::new().unwrap();
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/Videos/7/stream?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let body = test::read_body(resp).await;
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(body.len(), on_disk.len());
}

#[actix_web::test]
async fn start_time_ticks_returns_206_starting_at_byte_offset() {
    let td = TempDir::new().unwrap();
    // 1 Mbps × 10s = 1_250_000 bytes total. 1s in = byte 125_000.
    let (state, token, path) = seed(td.path(), 1_000_000, 10_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

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
    // Bytes must match the source from the offset onwards.
    assert_eq!(&body[..256], &on_disk[125_000..125_256]);
}

#[actix_web::test]
async fn range_header_wins_over_start_time_ticks() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path(), 1_000_000, 10_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // Range covers bytes 100-199 (100 bytes); StartTimeTicks would
    // ask for byte 125_000 if applied. NamedFile should pick Range.
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

#[actix_web::test]
async fn start_time_ticks_past_eof_returns_416() {
    let td = TempDir::new().unwrap();
    let (state, token, _) = seed(td.path(), 1_000_000, 10_000).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // 100 seconds into a 10-second source.
    let past_eof = 100 * TICKS_PER_SECOND;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Videos/7/stream?api_key={token}&StartTimeTicks={past_eof}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 416, "{}", resp.status());
}
