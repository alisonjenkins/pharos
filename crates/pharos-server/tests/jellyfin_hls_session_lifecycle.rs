#![allow(clippy::unwrap_used, clippy::expect_used)]
//! W4 — PlaySessionId enforcement on HLS segment + cache hits.
//!
//! Real-ffmpeg, fixture-gated (`#[ignore]`). Covers the security
//! property the plan calls out: a cached `.ts` on disk must NOT serve
//! after the play session has been invalidated (GC'd, expired, or
//! explicitly removed via `/Sessions/Playing/Stopped`).
//!
//! Flow:
//!   1. Seed a `TranscodeSession` under `PlaySessionId=psid1`.
//!   2. GET /videos/{id}/hls1/main/0.ts?PlaySessionId=psid1 → 200,
//!      transcode runs, segment lands in the on-disk LRU.
//!   3. `TranscodeSessionRegistry::remove("psid1")`.
//!   4. GET the same URL again → expect 410 Gone even though the
//!      `.ts` file is still on disk.
//!
//! Plus the legacy "no PlaySessionId at all" path still works (single-
//! variant clients that bypass the master playlist).

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{device_profile::Decision, hls},
    auth::BuiltinAuth,
    state::{AppState, Stores},
    transcode_sessions::TranscodeSession,
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
            has_primary_art: false,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        })
        .await
        .unwrap();
    let cache = HlsSegmentCache::new(cache_dir, 32 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn segment_410s_after_session_removed_even_when_cached() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/fixture missing");
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;

    // Seed the session under PSID=psid1.
    state
        .transcode_sessions
        .insert(
            "psid1".into(),
            TranscodeSession {
                media_id: 7,
                decision: Decision::Transcode {
                    target_container: "mpegts".into(),
                    target_video_codec: Some("h264".into()),
                    target_audio_codec: Some("aac".into()),
                    max_video_bitrate_bps: Some(500_000),
                },
                source_probe: MediaProbe {
                    duration_ms: Some(3_000),
                    width: Some(320),
                    height: Some(240),
                    bitrate_bps: Some(500_000),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();

    let app = test::init_service(App::new().app_data(state.clone()).configure(hls::register)).await;

    // First fetch — warm cache, expect 200.
    let req1 = test::TestRequest::get()
        .uri(&format!(
            "/videos/7/hls1/main/0.ts?PlaySessionId=psid1&api_key={token}"
        ))
        .to_request();
    let resp1 = test::call_service(&app, req1).await;
    assert!(
        resp1.status().is_success(),
        "first fetch failed: {}",
        resp1.status()
    );

    // Sanity: a `.ts` is now on disk (single nested dir under the
    // cache root keyed by media_id).
    fn has_ts(p: &std::path::Path) -> bool {
        let Ok(rd) = std::fs::read_dir(p) else {
            return false;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() && has_ts(&path) {
                return true;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("ts") {
                return true;
            }
        }
        false
    }
    assert!(
        has_ts(td.path()),
        "expected a cached .ts under {:?}",
        td.path()
    );

    // Invalidate the session.
    state.transcode_sessions.remove("psid1").await.unwrap();

    // Second fetch — same URL — must 410 even though the file is on
    // disk. This is the W4 contract.
    let req2 = test::TestRequest::get()
        .uri(&format!(
            "/videos/7/hls1/main/0.ts?PlaySessionId=psid1&api_key={token}"
        ))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(
        resp2.status().as_u16(),
        410,
        "expected 410 Gone, got {}",
        resp2.status()
    );
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn segment_without_play_session_id_still_works() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // Legacy single-variant path — client hits main.m3u8 cold without
    // running PlaybackInfo first. Must still serve.
    let req = test::TestRequest::get()
        .uri(&format!("/videos/7/hls1/main/0.ts?api_key={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "legacy no-PSID path failed: {}",
        resp.status()
    );
}

#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn segment_410s_when_play_session_id_was_never_registered() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let (state, token) = seed(td.path()).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // PSID present but no insert happened — i.e. forged URL or replay
    // after a server restart wiped the in-memory registry.
    let req = test::TestRequest::get()
        .uri(&format!(
            "/videos/7/hls1/main/0.ts?PlaySessionId=nope&api_key={token}"
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        410,
        "expected 410, got {}",
        resp.status()
    );
}
