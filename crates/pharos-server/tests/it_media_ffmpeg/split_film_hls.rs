//! Spec 037 — a film split across two files streams as one: its playlists run
//! on into the second part after a discontinuity, and the second part's init
//! and segments come from the second file, through real ffmpeg (V1154).

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    AlternateMediaSource, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::hls,
    auth::BuiltinAuth,
    state::{AppState, Stores},
};
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

/// A 9 s VP9/Opus clip of `pattern`, so each part lists two segments.
fn make_part(dir: &Path, name: &str, pattern: &str) -> PathBuf {
    let out = dir.join(name);
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("{pattern}=duration=9:size=320x240:rate=24"))
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=9",
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

fn probe() -> MediaProbe {
    MediaProbe {
        duration_ms: Some(9_000),
        width: Some(320),
        height: Some(240),
        bitrate_bps: Some(400_000),
        frame_rate_mille: Some(24_000),
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        ..Default::default()
    }
}

async fn seed(first: PathBuf, second: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
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
            path: first,
            title: "split".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                alternate_sources: vec![AlternateMediaSource {
                    id: "part-2".into(),
                    path: second,
                    duration_ms: Some(9_000),
                    name: Some("Part 2".into()),
                    probe: Some(Box::new(probe())),
                    part: Some(2),
                    ..Default::default()
                }],
                ..probe()
            },
            ..Default::default()
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

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9 + libopus) on PATH"]
async fn a_split_film_streams_on_into_its_second_part() {
    if !ffmpeg_ok() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let first = make_part(td.path(), "split cd1.webm", "testsrc");
    let second = make_part(td.path(), "split cd2.webm", "smptebars");
    let (state, token) = seed(first, second, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;
    let get = |uri: String| test::TestRequest::get().uri(&uri).to_request();

    let variant = test::call_and_read_body(
        &app,
        get(format!("/videos/42/vp9/main.m3u8?api_key={token}")),
    )
    .await;
    let variant = String::from_utf8(variant.to_vec()).unwrap();
    let lines: Vec<&str> = variant.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "#EXT-X-DISCONTINUITY")
        .unwrap_or_else(|| panic!("no discontinuity before part 2:\n{variant}"));
    let maps: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("#EXT-X-MAP"))
        .collect();
    assert_eq!(maps.len(), 2, "{variant}");
    let part_init = maps[1]
        .trim_start_matches("#EXT-X-MAP:URI=\"")
        .trim_end_matches('"')
        .to_string();
    assert!(part_init.contains("-part-2"), "{variant}");
    let part_segs: Vec<&str> = lines[at..]
        .iter()
        .copied()
        .filter(|l| l.starts_with("/videos/"))
        .collect();
    assert_eq!(part_segs.len(), 2, "{variant}");

    let init = test::call_and_read_body(&app, get(part_init.clone())).await;
    assert!(
        has_box(&init, b"moov") && !has_box(&init, b"moof"),
        "part 2 init"
    );
    let seg = test::call_service(&app, get(part_segs[1].to_string())).await;
    assert!(
        seg.status().is_success(),
        "part 2 segment 1: {}",
        seg.status()
    );
    let body = test::read_body(seg).await;
    assert!(
        has_box(&body, b"moof") && has_box(&body, b"mdat"),
        "part 2 media"
    );

    // Part 2 has two segments of its own; a third is past its end.
    let past = part_segs[1].replacen("/1.m4s?", "/2.m4s?", 1);
    let past = test::call_service(&app, get(past)).await;
    assert_eq!(past.status(), actix_web::http::StatusCode::NOT_FOUND);

    // The VP9 path's separate audio rendition runs on into part 2 in step.
    let audio = test::call_and_read_body(
        &app,
        get(format!("/videos/42/vp9/audio.m3u8?api_key={token}")),
    )
    .await;
    let audio = String::from_utf8(audio.to_vec()).unwrap();
    let audio_part: Vec<&str> = audio
        .lines()
        .skip_while(|l| *l != "#EXT-X-DISCONTINUITY")
        .filter(|l| l.starts_with("/videos/"))
        .collect();
    assert_eq!(audio_part.len(), 2, "{audio}");
    assert!(audio_part[1].contains("-part-2"), "{audio}");
    let seg = test::call_service(&app, get(audio_part[1].to_string())).await;
    assert!(
        seg.status().is_success(),
        "part 2 audio segment: {}",
        seg.status()
    );
    assert!(
        has_box(&test::read_body(seg).await, b"moof"),
        "part 2 audio media"
    );

    // The mpegts surface runs on into part 2 as well.
    let ts =
        test::call_and_read_body(&app, get(format!("/videos/42/main.m3u8?api_key={token}"))).await;
    let ts = String::from_utf8(ts.to_vec()).unwrap();
    let ts_part: Vec<&str> = ts
        .lines()
        .skip_while(|l| *l != "#EXT-X-DISCONTINUITY")
        .filter(|l| l.starts_with("/videos/"))
        .collect();
    assert_eq!(ts_part.len(), 2, "{ts}");
    let seg = test::call_service(&app, get(ts_part[0].to_string())).await;
    assert!(
        seg.status().is_success(),
        "part 2 ts segment: {}",
        seg.status()
    );
    assert!(!test::read_body(seg).await.is_empty());
}
