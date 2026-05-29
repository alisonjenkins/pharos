#![allow(clippy::unwrap_used, clippy::expect_used)]
//! W3 — multi-bitrate HLS variant routing.
//!
//! Drives the same fixture-gated, `#[ignore]` pattern as
//! `ffmpeg_integration.rs`. Skips outside the devShell (no
//! `PHAROS_TEST_FIXTURES`, no ffmpeg on PATH).
//!
//! Asserts:
//!   1. `GET /Videos/{id}/master.m3u8` returns ≥1 `EXT-X-STREAM-INF`
//!      entry per variant in the ladder (plus the legacy `main`).
//!   2. `GET /videos/{id}/hls1/720p/0.ts` and
//!      `GET /videos/{id}/hls1/480p/0.ts` both succeed AND produce
//!      different byte streams — proving the per-variant bitrate
//!      override threads through the cache key into ffmpeg.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_cache::HlsSegmentCache;
use pharos_server::{api::jellyfin::hls, auth::BuiltinAuth, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::PathBuf;
use tempfile::TempDir;

fn fixture(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("PHAROS_TEST_FIXTURES").map(PathBuf::from)?;
    let p = dir.join(name);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

fn ffmpeg_available() -> bool {
    fn ok(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    ok("ffmpeg") && ok("ffprobe") && fixture("dualaudio.mkv").is_some()
}

async fn seed_with_fixture(
    fixture_path: PathBuf,
    cache_dir: &std::path::Path,
) -> (web::Data<AppState>, String) {
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
            id: 42,
            path: fixture_path,
            title: "fixture".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(500_000),
                ..Default::default()
            },
            series: None,
            created_at: None,
        })
        .await
        .unwrap();
    let cache = HlsSegmentCache::new(cache_dir, 64 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn master_playlist_renders_variants_for_real_fixture() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/fixture missing");
        return;
    }
    let td = TempDir::new().unwrap();
    let fx = fixture("dualaudio.mkv").unwrap();
    let (state, token) = seed_with_fixture(fx, td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/videos/42/master.m3u8?api_key={token}"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    // Baseline + at least one ladder rung. 320x240 source → P360 only.
    let inf_count = s.matches("#EXT-X-STREAM-INF").count();
    assert!(
        inf_count >= 2,
        "expected ≥2 stream-inf, got {inf_count} in:\n{s}"
    );
    assert!(s.contains("/Videos/42/main.m3u8"), "missing baseline:\n{s}");
    assert!(
        s.contains("/Videos/42/variants/360p.m3u8"),
        "missing 360p:\n{s}"
    );
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn named_variants_produce_distinct_segment_bytes() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/fixture missing");
        return;
    }
    let td = TempDir::new().unwrap();
    let fx = fixture("dualaudio.mkv").unwrap();
    let (state, token) = seed_with_fixture(fx, td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    let req720 = test::TestRequest::get()
        .uri(&format!("/videos/42/hls1/720p/0.ts?api_key={token}"))
        .to_request();
    let body720 = test::call_and_read_body(&app, req720).await.to_vec();

    let req480 = test::TestRequest::get()
        .uri(&format!("/videos/42/hls1/480p/0.ts?api_key={token}"))
        .to_request();
    let body480 = test::call_and_read_body(&app, req480).await.to_vec();

    assert!(!body720.is_empty(), "720p variant produced no bytes");
    assert!(!body480.is_empty(), "480p variant produced no bytes");
    // Different bitrate caps + different on-disk cache keys → distinct
    // mpegts payloads. (Identical bytes here would mean the bitrate
    // override silently no-op'd or the cache collided.)
    assert_ne!(
        body720, body480,
        "720p vs 480p variant produced identical segment bytes"
    );
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn unknown_variant_returns_404() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fx = fixture("dualaudio.mkv").unwrap();
    let (state, token) = seed_with_fixture(fx, td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/videos/42/hls1/8k/0.ts?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}
