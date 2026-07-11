#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Ground-truth: does the VP9-in-fMP4 HLS path actually switch AUDIO tracks?
//!
//! jellyfin-web's audio dropdown re-requests the stream with `AudioStreamIndex`
//! set to the ABSOLUTE ffprobe stream index of the chosen track. The VP9
//! segment handler must map that to the per-codec `-map 0:a:N` selector so the
//! transcoded segment carries the RIGHT audio — not silently fall back to the
//! default (first) track, which is the reported "audio switching does nothing"
//! bug.
//!
//! This drives the real handlers through a real transcode of a 2-audio-track
//! source (track @300 Hz on absolute index 1, track @3000 Hz on index 2),
//! then decodes each segment and measures which tone is actually present.
//!
//! `#[ignore]` + ffmpeg-gated like the other real-transcode suites.

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    AudioTrack, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
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

/// 10 s clip: video on stream 0, a 300 Hz tone on audio stream 1, a 3000 Hz
/// tone on audio stream 2. The two tones are far apart so a band-energy probe
/// cleanly tells which track a transcoded segment carries.
fn make_two_audio_clip(dir: &Path) -> PathBuf {
    let out = dir.join("two_audio.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=10:size=320x240:rate=24",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=300:duration=10"])
        .args(["-f", "lavfi", "-i", "sine=frequency=3000:duration=10"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "ultrafast",
        ])
        .args(["-c:a", "aac"])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg two-audio clip generation failed");
    out
}

async fn seed(fixture: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
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
    // Two audio tracks at ABSOLUTE ffprobe indices 1 and 2 (video is 0). This
    // is exactly what jellyfin-web echoes back as `AudioStreamIndex`.
    let audio_tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            channels: Some(1),
            is_default: true,
            ..Default::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("aac".into()),
            channels: Some(1),
            ..Default::default()
        },
    ];
    stores
        .put(MediaItem {
            id: 42,
            path: fixture,
            title: "clip".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(10_000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(400_000),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                audio_tracks,
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

/// Mean volume (dBFS, negative; closer to 0 = louder) of `media` after a
/// narrow band-pass around `freq` Hz. Feeds bytes to ffmpeg on stdin.
fn band_energy_db(media: &[u8], freq: u32, dir: &Path) -> f64 {
    let f = dir.join(format!("probe_{freq}.mp4"));
    std::fs::write(&f, media).unwrap();
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "info", "-i"])
        .arg(&f)
        .args([
            "-af",
            &format!("bandpass=f={freq}:width_type=h:w=100,volumedetect"),
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("spawn ffmpeg volumedetect");
    let log = String::from_utf8_lossy(&out.stderr);
    // volumedetect prints e.g. "[Parsed_volumedetect_1 @ ..] mean_volume: -34.7 dB"
    log.lines()
        .find_map(|l| {
            l.split("mean_volume:")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|n| n.parse::<f64>().ok())
        })
        .unwrap_or_else(|| panic!("no mean_volume in ffmpeg log for {freq} Hz:\n{log}"))
}

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9 + libopus) on PATH"]
async fn vp9_audio_stream_index_selects_the_right_track() {
    if !ffmpeg_ok() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let clip = make_two_audio_clip(td.path());
    let (state, token) = seed(clip, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // For each audio track, fetch the shared init + segment 0 with that
    // track selected, concatenate (what hls.js feeds MSE), and measure the
    // 300 Hz vs 3000 Hz band energy. The SELECTED track's tone must dominate.
    for (abs_idx, present, absent) in [(1u32, 300u32, 3000u32), (2u32, 3000u32, 300u32)] {
        let init = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/videos/42/vp9/init.mp4?api_key={token}&AudioStreamIndex={abs_idx}"
                ))
                .to_request(),
        )
        .await;
        let seg = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/videos/42/vp9/0.m4s?api_key={token}&AudioStreamIndex={abs_idx}"
                ))
                .to_request(),
        )
        .await;
        let mut media = init.to_vec();
        media.extend_from_slice(&seg);

        let e_present = band_energy_db(&media, present, td.path());
        let e_absent = band_energy_db(&media, absent, td.path());
        assert!(
            e_present > e_absent + 12.0,
            "AudioStreamIndex={abs_idx}: expected the {present} Hz tone to dominate \
             (present {e_present:.1} dB vs absent {absent} Hz {e_absent:.1} dB). A <12 dB gap \
             means the handler served the DEFAULT audio track instead of the selected one."
        );
    }
}
