//! Verifies the continuous-audio-rendition A/V-sync fix END-TO-END at the
//! server artifact level (the real fix for per-segment Opus preskip drift):
//! the master playlist declares an AUDIO group the video variant references;
//! the audio rendition (one continuous ffmpeg) is GAPLESS + driftless; and
//! video segments are AUDIO-FREE. Player-side sync (hls.js) is validated in
//! the browser; this locks the server side so it can't silently regress.
//!
//! ffmpeg-gated + `#[ignore]` like the other real-transcode suites.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{api::jellyfin::hls, auth::BuiltinAuth, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const SECS: u32 = 42; // 7 audio segments
const SRC_RATE: &str = "24000/1001";

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Source with a 1 kHz beep 0.05 s into every second — the audio sync marker.
fn make_clip(dir: &Path) -> std::path::PathBuf {
    let out = dir.join("clip.mkv");
    let af = "[1:a]volume=enable='lt(mod(t\\,1)\\,0.05)':volume=1:eval=frame,\
              volume=enable='gte(mod(t\\,1)\\,0.05)':volume=0:eval=frame[a]";
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:s=320x240:r={SRC_RATE}:d={SECS}"),
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=1000:duration={SECS}"),
        ])
        .args(["-filter_complex", af])
        .args(["-map", "0:v", "-map", "[a]"])
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-ar",
            "48000",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "clip generation failed");
    out
}

/// Beep onset times (silence_end) from a file.
fn beep_times(file: &Path) -> Vec<f64> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(file)
        .args([
            "-vn",
            "-af",
            "silencedetect=n=-40dB:d=0.05",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("silencedetect");
    let s = String::from_utf8_lossy(&out.stderr);
    let mut v = Vec::new();
    for line in s.lines() {
        if let Some(i) = line.find("silence_end:") {
            let n: String = line[i + 12..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(x) = n.parse() {
                v.push(x);
            }
        }
    }
    v
}

async fn seed(fixture: std::path::PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
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
            title: "s".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(SECS as u64 * 1000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(400_000),
                video_codec: Some("h264".into()),
                audio_codec: Some("pcm".into()),
                frame_rate_mille: Some(23_976),
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

#[actix_web::test]
#[ignore = "requires ffmpeg; real transcode"]
async fn audio_rendition_is_gapless_and_video_is_audio_free() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not available — skipping");
        return;
    }
    let td = TempDir::new().unwrap();
    let src = make_clip(td.path());
    let (state, token) = seed(src, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    macro_rules! get {
        ($uri:expr) => {{
            let sep = if ($uri).contains('?') { "&" } else { "?" };
            test::call_and_read_body(
                &app,
                test::TestRequest::get()
                    .uri(&format!("{}{sep}api_key={token}", $uri))
                    .to_request(),
            )
            .await
        }};
    }

    // 1. Master playlist declares the audio group + the variant references it.
    let master = String::from_utf8_lossy(&get!("/videos/42/vp9/master.m3u8")).to_string();
    assert!(
        master.contains("TYPE=AUDIO"),
        "master lacks audio group:\n{master}"
    );
    assert!(
        master.contains("AUDIO=\"aud\""),
        "variant doesn't reference audio group:\n{master}"
    );

    // 2. Audio playlist lists init + segments.
    let aplaylist = String::from_utf8_lossy(&get!("/videos/42/vp9/audio.m3u8")).to_string();
    assert!(
        aplaylist.contains("audio/init.mp4"),
        "no audio init:\n{aplaylist}"
    );
    assert!(
        aplaylist.contains("audio/a0.m4s"),
        "no audio seg0:\n{aplaylist}"
    );

    // 3. Reassemble the audio rendition (init + all segments) and check it's
    //    GAPLESS: beeps land ~1s apart with no growing drift.
    let n_aud = SECS.div_ceil(6);
    let mut audio = get!("/videos/42/vp9/audio/init.mp4").to_vec();
    for n in 0..n_aud {
        let seg = get!(format!("/videos/42/vp9/audio/a{n}.m4s"));
        assert!(!seg.is_empty(), "audio segment {n} empty/not produced");
        audio.extend_from_slice(&seg);
    }
    let acat = td.path().join("audio.mp4");
    std::fs::write(&acat, &audio).unwrap();
    let beeps = beep_times(&acat);
    eprintln!("audio rendition beeps: {}", beeps.len());
    assert!(
        beeps.len() >= (SECS as usize - 3),
        "lost audio content: {} beeps of ~{SECS}",
        beeps.len()
    );
    // Drift: each beep should be near its integer second. Max deviation from
    // the nearest whole second stays tiny (constant preskip, no accumulation).
    let max_dev = beeps
        .iter()
        .map(|b| (b - b.round()).abs())
        .fold(0.0_f64, f64::max);
    eprintln!(
        "audio max deviation from whole-second grid = {:.0}ms",
        max_dev * 1000.0
    );
    assert!(
        max_dev < 0.1,
        "audio drifts off its own grid: {:.0}ms",
        max_dev * 1000.0
    );

    // 4. A video segment must be AUDIO-FREE.
    let vinit = get!("/videos/42/vp9/init.mp4").to_vec();
    let vseg = get!("/videos/42/vp9/0.m4s");
    let vcat = td.path().join("v.mp4");
    let mut vbytes = vinit.clone();
    vbytes.extend_from_slice(&vseg);
    std::fs::write(&vcat, &vbytes).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(&vcat)
        .output()
        .expect("ffprobe");
    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(
        streams.contains("video"),
        "video segment has no video:\n{streams}"
    );
    assert!(
        !streams.contains("audio"),
        "video segment still carries audio:\n{streams}"
    );
}
