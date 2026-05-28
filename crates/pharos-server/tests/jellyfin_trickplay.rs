#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Trickplay end-to-end. Real-ffmpeg gated via `PHAROS_TEST_FIXTURES`
//! + `#[ignore]`. Runs alongside the other ffmpeg-backed suites:
//!
//! ```sh
//! nix develop --command cargo nextest run --run-ignored only \
//!   -p pharos-server --test jellyfin_trickplay
//! ```
//!
//! Verifies:
//! 1. First GET on a tile spawns ffmpeg, returns a valid JPEG.
//! 2. Second GET on the same tile hits cache (no spawn — proven by
//!    pointing the cache at a missing ffmpeg binary after the first
//!    fetch warmed the disk).
//! 3. Unknown width 404s.
//! 4. Tile index past the layout's tile_count 404s.
//! 5. BaseItemDto.Trickplay carries the layout map advertising the
//!    configured widths.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::trickplay, auth::BuiltinAuth, state::AppState, trickplay_cache::TrickplayCache,
};
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
    ok("ffmpeg") && fixture("video.webm").is_some()
}

async fn seed(cache_dir: &std::path::Path) -> (web::Data<AppState>, String) {
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
    let fx = fixture("video.webm").unwrap();
    stores
        .put(MediaItem {
            id: 7,
            path: fx,
            title: "fx".into(),
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
    let cache = TrickplayCache::new(cache_dir, 32 * 1024 * 1024);
    let state = web::Data::new(
        AppState::new(stores, "t".into())
            .with_trickplay_cache(cache)
            .with_trickplay_layout(vec![320], 1_000), // 1s interval — fixture is 3s
    );
    (state, token.0.expose().to_string())
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn first_fetch_generates_then_second_fetch_hits_cache() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/fixture missing");
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .configure(trickplay::register),
    )
    .await;

    // 1. Miss → ffmpeg runs → JPEG returned with SOI marker.
    let req = test::TestRequest::get()
        .uri(&format!("/videos/7/trickplay/320/0.jpg?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "first fetch failed");
    let body = test::read_body(resp).await;
    assert!(body.len() > 256, "tile too small: {} bytes", body.len());
    assert_eq!(&body[..2], &[0xFF, 0xD8], "expected JPEG SOI");

    // 2. Replace the cache with one whose ffmpeg path is missing —
    // ensures the second call cannot transcode. The on-disk warm
    // segment must still serve it.
    let warm_state = web::Data::new(
        AppState::new(state.stores.clone(), "t".into())
            .with_trickplay_cache(
                TrickplayCache::new(td.path(), 32 * 1024 * 1024).with_ffmpeg("/no/such/ffmpeg"),
            )
            .with_trickplay_layout(vec![320], 1_000),
    );
    let app2 = test::init_service(
        App::new()
            .app_data(warm_state)
            .configure(trickplay::register),
    )
    .await;
    let req2 = test::TestRequest::get()
        .uri(&format!("/videos/7/trickplay/320/0.jpg?api_key={token}"))
        .to_request();
    let resp2 = test::call_service(&app2, req2).await;
    assert_eq!(resp2.status(), 200, "warm fetch failed");
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn unknown_width_404s() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(trickplay::register)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/videos/7/trickplay/8000/0.jpg?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn out_of_range_tile_index_404s() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(trickplay::register)).await;
    // Fixture is 3 s @ 1 s interval = 3 thumbs → 1 tile → only index 0
    // is valid. Tile index 99 must 404.
    let req = test::TestRequest::get()
        .uri(&format!("/videos/7/trickplay/320/99.jpg?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn dto_layout_map_advertises_configured_widths() {
    // Pure unit — no ffmpeg needed.
    let probe = MediaProbe {
        duration_ms: Some(180_000), // 3 min
        width: Some(1920),
        height: Some(1080),
        ..Default::default()
    };
    let map = trickplay::build_dto_layout_map(&probe, &[320], 10_000);
    assert!(map.contains_key("320"));
    let v = map.get("320").unwrap();
    assert_eq!(v.get("Width").unwrap().as_u64().unwrap(), 320);
    assert_eq!(v.get("Height").unwrap().as_u64().unwrap(), 180);
    assert_eq!(v.get("Interval").unwrap().as_u64().unwrap(), 10_000);
    // 180s / 10s = 18 thumbs.
    assert_eq!(v.get("ThumbnailCount").unwrap().as_u64().unwrap(), 18);
}
