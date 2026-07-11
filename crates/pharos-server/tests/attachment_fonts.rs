#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Embedded-font (attachment) delivery for ASS subtitles on the Firefox path.
//!
//! jellyfin-web renders ASS/SSA subs with SubtitlesOctopus (libass-in-WASM),
//! which fetches EVERY embedded font before it can draw a single cue (the
//! "Fetching assets" spinner). pharos extracted fonts one-at-a-time, and each
//! extraction re-opens the whole (multi-GB, NFS-backed) source to copy one
//! attachment — so a title with N fonts paid N cold opens on first playback
//! and the subtitles only appeared once every font finally landed in cache.
//!
//! This drives the real attachment route and asserts that requesting ONE font
//! warms the cache for ALL of them (one open, batch dump), which is what keeps
//! SubtitlesOctopus from stalling.
//!
//! ffmpeg-gated + `#[ignore]`: the fixture is muxed by `ffmpeg` and extraction
//! shells out / uses libav.

use actix_web::{test, web, App};
use pharos_cache::ImageCache;
use pharos_core::{
    MediaAttachment, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SubtitleTrack,
    TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Distinct bytes per font so a served body can be matched back to its source.
const FONTS: &[(&str, &[u8])] = &[
    ("zero.ttf", b"FONT-ZERO-payload-0000000000"),
    ("one.ttf", b"FONT-ONE-payload-11111111111"),
    ("two.ttf", b"FONT-TWO-payload-22222222222"),
];

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Mux a tiny clip carrying 3 embedded font attachments (plus video+audio).
/// Returns the `.mkv` path; attachment stream indices are discovered by the
/// caller via ffprobe (order is ffmpeg-version dependent).
fn make_media_with_fonts(dir: &Path) -> PathBuf {
    let mut font_paths = Vec::new();
    for (name, bytes) in FONTS {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        font_paths.push(p);
    }
    let out = dir.join("with_fonts.mkv");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=d=1:s=64x64:r=5"])
        .args(["-f", "lavfi", "-i", "sine=d=1"]);
    for p in &font_paths {
        cmd.arg("-attach").arg(p);
    }
    for (i, _) in FONTS.iter().enumerate() {
        cmd.arg(format!("-metadata:s:t:{i}"))
            .arg("mimetype=application/x-truetype-font");
    }
    cmd.args(["-map", "0:v", "-map", "1:a"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
        .arg(&out);
    let status = cmd.status().unwrap();
    assert!(status.success(), "ffmpeg font-mux failed");
    out
}

/// ffprobe the muxed file for its attachment stream indices, in order.
fn attachment_indices(file: &Path) -> Vec<u32> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "t",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

async fn seed(fixture: &Path, indices: &[u32], cache: ImageCache) -> web::Data<AppState> {
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
    let _token = stores.issue(uid, "t").await.unwrap();
    let attachments = indices
        .iter()
        .enumerate()
        .map(|(i, &idx)| MediaAttachment {
            stream_index: idx,
            filename: Some(FONTS[i].0.to_string()),
            mime_type: Some("application/x-truetype-font".into()),
            codec: Some("ttf".into()),
        })
        .collect();
    stores
        .put(MediaItem {
            id: 7,
            path: fixture.to_path_buf(),
            title: "m".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(1_000),
                attachments,
                subtitle_tracks: vec![SubtitleTrack {
                    stream_index: 2,
                    codec: Some("ass".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    web::Data::new(AppState::new(stores, "t".into()).with_image_cache(cache))
}

#[actix_web::test]
#[ignore = "spawns ffmpeg to mux + extract attachments"]
async fn one_font_request_warms_all_fonts() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let fixture = make_media_with_fonts(dir.path());
    let indices = attachment_indices(&fixture);
    assert_eq!(
        indices.len(),
        FONTS.len(),
        "fixture must carry 3 attachments"
    );

    let cache_root = dir.path().join("imgcache");
    let cache = ImageCache::new(&cache_root);
    let app = test::init_service(
        App::new()
            .app_data(seed(&fixture, &indices, cache).await)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    // jellyfin-web/SubtitlesOctopus requests the FIRST font. Mixed-case URL,
    // normalised by LowercasePath (real client shape).
    let first = indices[0];
    let req = test::TestRequest::get()
        .uri(&format!("/Videos/7/7/Attachments/{first}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = test::read_body(resp).await.to_vec();
    assert_eq!(body, FONTS[0].1, "served bytes must be the source font");

    // The single request must have warmed EVERY font (one open, batch dump):
    // SubtitlesOctopus's remaining N-1 fetches then hit warm cache instead of
    // each re-opening the multi-GB source.
    let attach_dir = cache_root.join("attachments").join("7");
    for &idx in &indices {
        let f = attach_dir.join(idx.to_string());
        assert!(
            f.exists(),
            "font {idx} must be warm after one request (batch extract); dir={attach_dir:?}"
        );
    }

    // And the warmed bytes must be correct per font.
    for (i, &idx) in indices.iter().enumerate() {
        let got = std::fs::read(attach_dir.join(idx.to_string())).unwrap();
        assert_eq!(got, FONTS[i].1, "warmed font {idx} bytes mismatch");
    }
}
