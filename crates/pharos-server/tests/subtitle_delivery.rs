#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T13 — end-to-end subtitle delivery across the various track types
//! jellyfin-web asks for, driven through the *real* HTTP handlers.
//!
//! The prior bugs were subtle wire-shape mismatches, so unit tests on the
//! DTO alone weren't enough — jellyfin-web fetches text subs out-of-band
//! (Stream.vtt / Stream.js / Stream.ass), and each type takes a different
//! path through ffmpeg. This test builds a real Matroska file carrying an
//! embedded **subrip** track (index 2) and an embedded **ASS** track
//! (index 3), then exercises every delivery endpoint the client uses:
//!
//! - subrip  → Stream.vtt (WebVTT), Stream.js (cue JSON), subtitles.srt
//! - ass     → Stream.ass (raw libass body), Stream.js (converted cues)
//! - image   → Stream.vtt / Stream.js both refuse with 415 (burn-in only)
//!
//! ffmpeg-gated + `#[ignore]`: the fixture is muxed by the `ffmpeg` binary,
//! and extraction spawns `ffmpeg` too. Run on demand with:
//!
//!   nix develop --command cargo nextest run --run-ignored only \
//!     -p pharos-server --test subtitle_delivery

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SubtitleTrack, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const SUBRIP_TEXT: &str = "Hello subrip";
const ASS_TEXT: &str = "World ass";

/// True when the `ffmpeg` binary resolves on PATH. Both fixture muxing and
/// the server-side extraction shell out to it; skip cleanly when absent so
/// the default (non-ignored) suite never depends on it.
fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Mux a 3s A/V clip carrying an embedded subrip track (output stream 2)
/// and an embedded ASS track (output stream 3). Returns the `.mkv` path.
/// Stream order is pinned by `-map` order: 0=video, 1=audio, 2=subrip,
/// 3=ass — matching the `SubtitleTrack.stream_index` values seeded below.
fn make_media_with_subs(dir: &Path) -> PathBuf {
    let srt = dir.join("sub.srt");
    std::fs::write(
        &srt,
        format!("1\n00:00:00,500 --> 00:00:02,500\n{SUBRIP_TEXT}\n"),
    )
    .unwrap();

    // Minimal but complete ASS: SubtitlesOctopus (libass) needs the script
    // header + a styled Dialogue line, which is exactly what the raw
    // Stream.ass endpoint must pass through verbatim.
    let ass = dir.join("sub.ass");
    std::fs::write(
        &ass,
        format!(
            "[Script Info]\n\
             ScriptType: v4.00+\n\
             PlayResX: 384\n\
             PlayResY: 288\n\
             \n\
             [V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n\
             \n\
             [Events]\n\
             Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
             Dialogue: 0,0:00:00.50,0:00:02.50,Default,,0,0,0,,{ASS_TEXT}\n"
        ),
    )
    .unwrap();

    let out = dir.join("with_subs.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=3:s=320x240:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=d=3",
            "-i",
            srt.to_str().unwrap(),
            "-i",
            ass.to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2",
            "-map",
            "3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "copy",
            out.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg fixture mux failed");
    out
}

/// Seed a single-item store whose media path is `fixture`, advertising a
/// subrip (2), ass (3) and image/PGS (4) subtitle track. Returns the wired
/// `AppState` (no subtitle cache → handlers spawn ffmpeg directly).
async fn seed(fixture: &Path) -> web::Data<AppState> {
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
    let _token = stores.issue(uid, "t").await.unwrap();
    stores
        .put(MediaItem {
            id: 7,
            path: fixture.to_path_buf(),
            title: "m".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_000),
                subtitle_tracks: vec![
                    SubtitleTrack {
                        stream_index: 2,
                        codec: Some("subrip".into()),
                        language: Some("eng".into()),
                        title: Some("English".into()),
                        is_default: true,
                        ..Default::default()
                    },
                    SubtitleTrack {
                        stream_index: 3,
                        codec: Some("ass".into()),
                        language: Some("eng".into()),
                        title: Some("Signs".into()),
                        ..Default::default()
                    },
                    // Synthetic image track — no real stream exists; the text
                    // routes must refuse it by codec *before* touching ffmpeg.
                    SubtitleTrack {
                        stream_index: 4,
                        codec: Some("hdmv_pgs_subtitle".into()),
                        language: Some("eng".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    web::Data::new(AppState::new(stores, "t".into()))
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state)
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

/// GET a public subtitle route, yielding `(status, content-type, body)`. No
/// Authorization header — mirrors how jellyfin-web's JS renderer fetches
/// these out-of-band, like the image routes.
macro_rules! get {
    ($app:expr, $uri:expr) => {{
        let req = test::TestRequest::get().uri($uri).to_request();
        let resp = test::call_service(&$app, req).await;
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = test::read_body(resp).await.to_vec();
        (status, ct, body)
    }};
}

#[actix_web::test]
#[ignore = "spawns ffmpeg to mux + extract"]
async fn subrip_track_delivers_webvtt_and_srt() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let fixture = make_media_with_subs(dir.path());
    let app = app!(seed(&fixture).await);

    // Stream.vtt — mixed-case client URL, normalised by LowercasePath.
    let (status, ct, body) = get!(app, "/Videos/7/7/Subtitles/2/Stream.vtt");
    assert_eq!(status, 200, "vtt body: {}", String::from_utf8_lossy(&body));
    assert!(ct.contains("vtt"), "unexpected content-type {ct}");
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("WEBVTT"), "not WebVTT: {text}");
    assert!(text.contains(SUBRIP_TEXT), "cue text missing: {text}");
    assert!(text.contains("-->"), "no cue timing: {text}");

    // subtitles.srt — legacy Roku/Android form.
    let (status, _ct, body) = get!(app, "/Videos/7/7/Subtitles/2/subtitles.srt");
    assert_eq!(status, 200);
    let srt = String::from_utf8_lossy(&body);
    assert!(srt.contains(SUBRIP_TEXT), "srt cue missing: {srt}");
    assert!(srt.contains("-->"), "srt timing missing: {srt}");
}

#[actix_web::test]
#[ignore = "spawns ffmpeg to mux + extract"]
async fn subrip_track_delivers_stream_js_cues() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let fixture = make_media_with_subs(dir.path());
    let app = app!(seed(&fixture).await);

    let (status, ct, body) = get!(app, "/Videos/7/7/Subtitles/2/Stream.js");
    assert_eq!(status, 200, "js body: {}", String::from_utf8_lossy(&body));
    assert!(ct.contains("json"), "unexpected content-type {ct}");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v
        .get("TrackEvents")
        .and_then(|e| e.as_array())
        .expect("TrackEvents array");
    assert!(!events.is_empty(), "no cue events: {v}");
    let first = &events[0];
    assert!(
        first["Text"]
            .as_str()
            .unwrap_or_default()
            .contains(SUBRIP_TEXT),
        "cue text missing: {first}"
    );
    // Ticks are 100ns units; the 0.5s→2.5s cue must be positive + ordered.
    let start = first["StartPositionTicks"].as_i64().unwrap();
    let end = first["EndPositionTicks"].as_i64().unwrap();
    assert!(start > 0 && end > start, "bad tick range {start}..{end}");
}

#[actix_web::test]
#[ignore = "spawns ffmpeg to mux + extract"]
async fn ass_track_delivers_raw_ass_and_js() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let fixture = make_media_with_subs(dir.path());
    let app = app!(seed(&fixture).await);

    // Stream.ass — raw libass body, headers intact.
    let (status, ct, body) = get!(app, "/Videos/7/7/Subtitles/3/Stream.ass");
    assert_eq!(status, 200, "ass body: {}", String::from_utf8_lossy(&body));
    assert!(
        ct.contains("ssa") || ct.contains("ass"),
        "content-type {ct}"
    );
    let ass = String::from_utf8_lossy(&body);
    assert!(ass.contains("[Script Info]"), "no ASS header: {ass}");
    assert!(ass.contains("Dialogue:"), "no Dialogue line: {ass}");
    assert!(ass.contains(ASS_TEXT), "dialogue text missing: {ass}");

    // The same ASS track is also offered as Stream.js (ass→vtt→cues) for
    // clients without SubtitlesOctopus.
    let (status, _ct, body) = get!(app, "/Videos/7/7/Subtitles/3/Stream.js");
    assert_eq!(status, 200, "js body: {}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["TrackEvents"].as_array().expect("TrackEvents");
    assert!(
        events
            .iter()
            .any(|e| e["Text"].as_str().unwrap_or_default().contains(ASS_TEXT)),
        "ass cue text missing from Stream.js: {v}"
    );
}

#[actix_web::test]
#[ignore = "spawns ffmpeg to mux + extract"]
async fn image_subtitle_refused_on_text_routes() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let fixture = make_media_with_subs(dir.path());
    let app = app!(seed(&fixture).await);

    // PGS is image-based — it burns into the transcode, never a text track.
    // Both text endpoints must refuse (415) by codec, not 500 on a bad map.
    let (status, _ct, _body) = get!(app, "/Videos/7/7/Subtitles/4/Stream.vtt");
    assert_eq!(status, 415, "PGS vtt should be Unsupported Media Type");
    let (status, _ct, _body) = get!(app, "/Videos/7/7/Subtitles/4/Stream.js");
    assert_eq!(status, 415, "PGS js should be Unsupported Media Type");
}
