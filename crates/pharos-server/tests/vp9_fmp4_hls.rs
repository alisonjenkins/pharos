#![allow(clippy::unwrap_used, clippy::expect_used)]
//! VP9-in-fMP4 HLS end-to-end (Firefox/Zen playback path).
//!
//! The H.264/MPEG-TS ladder is useless to Firefox (no H.264 in MSE), so those
//! clients get VP9 as fMP4 HLS. This drives the real handlers through a real
//! ffmpeg transcode and asserts the wire shape hls.js needs:
//!   1. master → advertises `vp09` + points at the VP9 variant.
//!   2. variant → VOD playlist with an `EXT-X-MAP` init + `.m4s` segments.
//!   3. init.mp4 → `ftyp`+`moov`, NO `moof` (a valid shared init segment).
//!   4. `{seg}.m4s` → `moof`+`mdat`, NO `moov`/`ftyp` (moof-only media).
//!   5. segment N's `tfdt` == N·6·timescale — the timeline correction that
//!      lets independently-transcoded segments concatenate + seek. This is the
//!      whole reason the path exists, verified through real ffmpeg output.
//!
//! `#[ignore]` + ffmpeg-gated like the other real-transcode suites; the clip
//! is generated in-test via lavfi so no fixture corpus is required.

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{api::jellyfin::hls, auth::BuiltinAuth, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn ffmpeg_ok() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Generate a 15 s VP9/Opus clip so the playlist lists ≥3 segments and
/// segment 2 (start 12 s) is inside the file.
fn make_clip(dir: &Path) -> PathBuf {
    let out = dir.join("clip.webm");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=15:size=320x240:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=15",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "300k",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-c:a",
            "libopus",
        ])
        .arg(&out)
        .arg("-y")
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg clip generation failed");
    out
}

async fn seed(fixture: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
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
            path: fixture,
            title: "clip".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(15_000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(400_000),
                video_codec: Some("vp9".into()),
                audio_codec: Some("opus".into()),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    let cache = HlsSegmentCache::new(cache_dir, 128 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

fn has_box(data: &[u8], fourcc: &[u8; 4]) -> bool {
    data.windows(4).any(|w| w == fourcc)
}

/// Byte-scan for every version-1 `tfdt` and return its 64-bit base decode time.
fn tfdt_values(data: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 16 <= data.len() {
        if &data[i..i + 4] == b"tfdt" && data[i + 4] == 1 {
            out.push(u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()));
        }
        i += 1;
    }
    out
}

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9 + libopus) on PATH"]
async fn vp9_fmp4_path_serves_seekable_hls() {
    if !ffmpeg_ok() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let clip = make_clip(td.path());
    let (state, token) = seed(clip, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // 1. Master advertises vp09 + routes to the VP9 variant.
    let master = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/master.m3u8?api_key={token}"))
            .to_request(),
    )
    .await;
    let master = std::str::from_utf8(&master).unwrap();
    assert!(
        master.contains("vp09"),
        "master must advertise vp09:\n{master}"
    );
    assert!(
        master.contains("/videos/42/vp9/main.m3u8"),
        "master must route to the VP9 variant:\n{master}"
    );

    // 2. Variant is a VOD fMP4 playlist: EXT-X-MAP init + .m4s segments.
    let variant = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/main.m3u8?api_key={token}"))
            .to_request(),
    )
    .await;
    let variant = std::str::from_utf8(&variant).unwrap();
    assert!(variant.contains("#EXT-X-VERSION:7"), "{variant}");
    assert!(
        variant.contains("#EXT-X-MAP:URI=\"/videos/42/vp9/init.mp4"),
        "variant must declare the fMP4 init:\n{variant}"
    );
    assert!(variant.contains("/videos/42/vp9/0.m4s"), "{variant}");
    assert!(
        variant.contains("/videos/42/vp9/2.m4s"),
        "15s/6s ⇒ ≥3 segs:\n{variant}"
    );

    // 3. init.mp4 is ftyp+moov, no moof.
    let init = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/init.mp4?api_key={token}"))
            .to_request(),
    )
    .await;
    assert!(
        has_box(&init, b"ftyp") && has_box(&init, b"moov"),
        "init needs ftyp+moov"
    );
    assert!(!has_box(&init, b"moof"), "init must not contain moof");

    // 4. Segment 0 is moof-only media (no moov/ftyp), tfdt at 0.
    let seg0 = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/0.m4s?api_key={token}"))
            .to_request(),
    )
    .await;
    assert!(
        has_box(&seg0, b"moof") && has_box(&seg0, b"mdat"),
        "seg0 needs moof+mdat"
    );
    assert!(
        !has_box(&seg0, b"moov"),
        "media segment must not carry moov"
    );
    assert!(!has_box(&seg0, b"mfra"), "stale mfra must be stripped");
    // A 6 s segment can hold >1 fragment; tfdts interleave [video, audio, …]
    // per fragment (moov track order). The FIRST fragment sits at the origin.
    let seg0_tfdts = tfdt_values(&seg0);
    assert!(
        seg0_tfdts.len() >= 2,
        "seg0 needs ≥1 fragment: {seg0_tfdts:?}"
    );
    assert_eq!(
        (seg0_tfdts[0], seg0_tfdts[1]),
        (0, 0),
        "seg0's first fragment starts at tfdt 0: {seg0_tfdts:?}"
    );

    // 5. Segment 2's first fragment sits at 12 s — the correction that makes
    //    independently-transcoded fMP4 segments concatenate + seek. Opus in mp4
    //    is always timescale 48000, so the first-fragment audio tfdt is a fixed
    //    12·48000; video timescale is encoder-chosen so just assert it shifted.
    let seg2 = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/2.m4s?api_key={token}"))
            .to_request(),
    )
    .await;
    let seg2_tfdts = tfdt_values(&seg2);
    assert!(
        seg2_tfdts.len() >= 2,
        "seg2 needs ≥1 fragment: {seg2_tfdts:?}"
    );
    assert_eq!(
        seg2_tfdts[1],
        12 * 48_000,
        "seg2 first-fragment audio tfdt must be 12s·48000=576000: {seg2_tfdts:?}"
    );
    assert!(
        seg2_tfdts[0] > 0,
        "seg2 first-fragment video tfdt must be shifted off 0: {seg2_tfdts:?}"
    );
}
