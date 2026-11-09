#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec 035 — a client's `MaxWidth` / `MaxHeight` (and a ladder rung's height)
//! shrink the HLS segments a 4K source is transcoded to, and an unbounded
//! request still gets the full frame.
//!
//! Needs `ffmpeg` / `ffprobe` on PATH (devShell and CI have them); the 4K
//! source is synthesised, so no fixture directory is involved.

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::hls,
    auth::BuiltinAuth,
    state::{AppState, Stores},
};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn make_4k_source(dir: &Path) -> PathBuf {
    let src = dir.join("uhd.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=2:size=3840x2160:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            "-y",
        ])
        .arg(&src)
        .output()
        .expect("spawn ffmpeg (is it on PATH? run inside the devShell)");
    assert!(
        out.status.success(),
        "source synth failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    src
}

fn frame_size(bytes: &[u8], dir: &Path, name: &str) -> (u32, u32) {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(&path)
        .output()
        .expect("spawn ffprobe");
    let text = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let (w, h) = text
        .split_once('x')
        .unwrap_or_else(|| panic!("ffprobe gave {text:?} for {name} ({} bytes)", bytes.len()));
    (
        w.parse()
            .unwrap_or_else(|_| panic!("bad width in {text:?}")),
        h.parse()
            .unwrap_or_else(|_| panic!("bad height in {text:?}")),
    )
}

async fn seed(src: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
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
            id: 42,
            path: src,
            title: "uhd".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe {
                duration_ms: Some(2_000),
                width: Some(3840),
                height: Some(2160),
                bitrate_bps: Some(8_000_000),
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
    let cache = HlsSegmentCache::new(cache_dir, 256 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

macro_rules! segment_size {
    ($app:expr, $uri:expr, $dir:expr, $name:expr $(,)?) => {{
        let uri: &str = $uri;
        let body =
            test::call_and_read_body(&$app, test::TestRequest::get().uri(uri).to_request()).await;
        assert!(!body.is_empty(), "{uri} produced no bytes");
        frame_size(&body, $dir, $name)
    }};
}

#[actix_web::test]
async fn max_width_downscales_a_4k_source_and_unbounded_requests_do_not() {
    let td = TempDir::new().unwrap();
    let src = make_4k_source(td.path());
    let cache = td.path().join("cache");
    let (state, token) = seed(src, &cache).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    let full = segment_size!(
        app,
        &format!("/videos/42/hls1/main/0.ts?api_key={token}"),
        td.path(),
        "full.ts",
    );
    assert_eq!(
        full,
        (3840, 2160),
        "control: no bound keeps the source size"
    );

    let bounded = segment_size!(
        app,
        &format!("/videos/42/hls1/main/0.ts?api_key={token}&MaxWidth=1920"),
        td.path(),
        "bounded.ts",
    );
    assert!(
        bounded.0 <= 1920 && bounded.1 <= 1080,
        "MaxWidth=1920 gave {bounded:?}"
    );
    assert_eq!(bounded.0 % 2 + bounded.1 % 2, 0, "even edges: {bounded:?}");
    assert_eq!(bounded.0 * 9, bounded.1 * 16, "aspect kept: {bounded:?}");

    // The unbounded request is still served from its own cache entry.
    let again = segment_size!(
        app,
        &format!("/videos/42/hls1/main/0.ts?api_key={token}"),
        td.path(),
        "again.ts",
    );
    assert_eq!(
        again,
        (3840, 2160),
        "a bounded request must not poison the unbounded key"
    );
}

#[actix_web::test]
async fn a_ladder_rung_and_max_height_both_bound_the_frame() {
    let td = TempDir::new().unwrap();
    let src = make_4k_source(td.path());
    let cache = td.path().join("cache");
    let (state, token) = seed(src, &cache).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    let rung = segment_size!(
        app,
        &format!("/videos/42/hls1/720p/0.ts?api_key={token}"),
        td.path(),
        "rung.ts",
    );
    assert_eq!(rung, (1280, 720), "the 720p rung is 720 tall");

    let tighter = segment_size!(
        app,
        &format!("/videos/42/hls1/720p/0.ts?api_key={token}&MaxHeight=480"),
        td.path(),
        "tighter.ts",
    );
    assert!(
        tighter.1 <= 480,
        "the client bound wins when tighter: {tighter:?}"
    );
}
