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
//! 1. `generates_valid_sprite_grid_from_inline_clip` — generate a sprite sheet
//!    (self-contained inline clip, ffmpeg-gated only) then serve a valid JPEG
//!    grid through the real tile route. The route is cache-only (generation is
//!    a background job, never on the request path), so the test pre-generates
//!    via `ensure_generated`, then asserts the served tile.
//! 2. `item_dto_trickplay_is_nested_by_media_source_id` — the full serialized
//!    BaseItemDto carries the NESTED `Trickplay[mediaSourceId][width]` shape
//!    jellyfin-web needs (regression guard for the invisible-previews bug).
//! 3. Unknown width 404s.
//! 4. Tile index past the layout's tile_count 404s.
//! 5. `dto_layout_map_advertises_configured_widths` — the inner width→info map.

use actix_web::{test, web, App};
use pharos_cache::TrickplayCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{self, trickplay},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
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
            metadata: Default::default(),
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

/// Generate a self-contained ~5 s clip so the generation path is exercised in
/// CI without an external `PHAROS_TEST_FIXTURES` video.
fn make_clip(dir: &std::path::Path) -> PathBuf {
    let out = dir.join("clip.webm");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=5:size=320x240:rate=10",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "200k",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
        ])
        .arg(&out)
        .arg("-y")
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "clip generation failed");
    out
}

fn ffmpeg_only() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9) on PATH"]
async fn generates_valid_sprite_grid_from_inline_clip() {
    // The full path the user cares about: a scrub request generates a sprite
    // sheet and serves a valid JPEG grid. Self-contained (inline clip), so it
    // runs in CI wherever ffmpeg is present — no external fixture needed.
    if !ffmpeg_only() {
        eprintln!("skipping: ffmpeg missing");
        return;
    }
    let td = TempDir::new().unwrap();
    let clip = make_clip(td.path());
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
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();
    stores
        .put(MediaItem {
            id: 7,
            path: clip.clone(),
            title: "fx".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(5_000),
                width: Some(320),
                height: Some(240),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let cache = TrickplayCache::new(td.path().join("cache"), 32 * 1024 * 1024);
    // The tile route serves cache-only (generation is a background job, never
    // on the request path — see trickplay.rs). So exercise the real generation
    // API first, then assert the route serves the produced grid.
    let probe = MediaStore::get(&stores, 7).await.unwrap().probe;
    let layout = pharos_jellyfin_api::dto::build_layout(&probe, 320, 1_000).unwrap();
    cache
        .ensure_generated(7, layout, &clip)
        .await
        .expect("trickplay generation failed");
    let state = web::Data::new(
        AppState::new(stores, "t".into())
            .with_trickplay_cache(cache)
            .with_trickplay_layout(vec![320], 1_000),
    );
    let app = test::init_service(App::new().app_data(state).configure(trickplay::register)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/7/trickplay/320/0.jpg?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200, "tile serve after generation failed");
    let body = test::read_body(resp).await;
    assert_eq!(&body[..2], &[0xFF, 0xD8], "expected JPEG SOI");
    assert!(
        body.len() > 1024,
        "sprite grid too small: {} bytes",
        body.len()
    );
}

#[actix_web::test]
async fn item_dto_trickplay_is_nested_by_media_source_id() {
    // Regression guard for the wire-shape bug that made previews invisible in
    // jellyfin-web: the client reads `item.Trickplay[mediaSourceId][width]`.
    // The pre-fix flat `{ width -> info }` map left that lookup undefined so the
    // client never requested a tile. Assert the FULL serialized BaseItemDto —
    // not just the inner builder — carries the nested shape.
    use pharos_core::{MediaItem, MediaKind};
    use pharos_jellyfin_api::dto::BaseItemDto;
    let item = MediaItem {
        id: 7,
        path: "/m/x.mkv".into(),
        title: "x".into(),
        kind: MediaKind::Movie,
        probe: MediaProbe {
            duration_ms: Some(180_000),
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        },
        ..Default::default()
    };
    let dto =
        BaseItemDto::from_domain(&item, "srv").with_trickplay(&item.probe, &[320, 640], 10_000);
    let v = serde_json::to_value(&dto).unwrap();
    assert_eq!(
        v["Trickplay"]["00000000000000000000000000000007"]["320"]["Width"]
            .as_u64()
            .unwrap(),
        320
    );
    assert_eq!(
        v["Trickplay"]["00000000000000000000000000000007"]["640"]["Width"]
            .as_u64()
            .unwrap(),
        640
    );
    assert!(
        v["Trickplay"].get("320").is_none(),
        "flat wire shape regressed (width at top level): {}",
        v["Trickplay"]
    );
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
    let map = pharos_jellyfin_api::dto::build_dto_layout_map(&probe, &[320], 10_000);
    assert!(map.contains_key("320"));
    let v = map.get("320").unwrap();
    assert_eq!(v.get("Width").unwrap().as_u64().unwrap(), 320);
    assert_eq!(v.get("Height").unwrap().as_u64().unwrap(), 180);
    assert_eq!(v.get("Interval").unwrap().as_u64().unwrap(), 10_000);
    // 180s / 10s = 18 thumbs.
    assert_eq!(v.get("ThumbnailCount").unwrap().as_u64().unwrap(), 18);
}

#[actix_web::test]
async fn http_get_item_emits_nested_trickplay() {
    // End-to-end wire guard, no ffmpeg: the real `GET /Items/{id}` handler
    // must call `with_trickplay` using the server's configured widths so
    // jellyfin-web receives `item.Trickplay[mediaSourceId][width]`. The
    // DTO-only test (`item_dto_trickplay_is_nested_by_media_source_id`)
    // exercises the builder in isolation and the tile test exercises the
    // image route — neither catches a handler that forgot the call or an
    // empty `state.trickplay_widths` (a no-op `with_trickplay`), which is
    // exactly what makes previews silently invisible. The layout is
    // probe-derived, so no fixture / ffmpeg is needed.
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
            id: 7,
            path: "/m/x.mkv".into(),
            title: "x".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(180_000),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    // B35 — the DTO must advertise ONLY widths whose tiles are actually on
    // disk. Seed tile 0 for width 320 (item 7) and nothing for width 640 or
    // for item 8: the DTO carries "320" only, and item 8 omits Trickplay
    // entirely (clients otherwise render an empty scrub-preview box against
    // 404 tiles).
    stores
        .put(MediaItem {
            id: 8,
            path: "/m/y.mkv".into(),
            title: "y".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(180_000),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let cache_dir = TempDir::new().unwrap();
    // Match the cache's generation marker so construction doesn't wipe the
    // seeded tile (reconcile_generation clears an unversioned root).
    std::fs::write(cache_dir.path().join(".gen_version"), "1").unwrap();
    std::fs::create_dir_all(cache_dir.path().join("7/320")).unwrap();
    std::fs::write(cache_dir.path().join("7/320/0.jpg"), b"jpg").unwrap();
    let cache = TrickplayCache::new(cache_dir.path(), u64::MAX);
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_trickplay_cache(cache)
            .with_trickplay_layout(vec![320, 640], 10_000),
    );
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri("/Items/7")
        .insert_header(("X-Emby-Token", token.0.expose().to_string()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        v["Trickplay"]["00000000000000000000000000000007"]["320"]["Width"]
            .as_u64()
            .unwrap(),
        320,
        "GET /Items/7 dropped the nested Trickplay map: {v}"
    );
    // Width 640 has NO tiles on disk → must NOT be advertised (B35).
    assert!(
        v["Trickplay"]["00000000000000000000000000000007"]
            .get("640")
            .is_none(),
        "ungenerated width advertised: {}",
        v["Trickplay"]
    );
    // Guard the pre-fix flat shape can't creep back at the HTTP layer either.
    assert!(
        v["Trickplay"].get("320").is_none(),
        "flat wire shape regressed: {}",
        v["Trickplay"]
    );

    // Item 8: nothing generated → no Trickplay field at all.
    let req = test::TestRequest::get()
        .uri("/Items/8")
        .insert_header(("X-Emby-Token", token.0.expose().to_string()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        v.get("Trickplay").is_none() || v["Trickplay"].as_object().is_some_and(|m| m.is_empty()),
        "item with no tiles must not advertise Trickplay: {}",
        v["Trickplay"]
    );
}
