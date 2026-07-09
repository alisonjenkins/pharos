//! Objective A/V-sync test for the VP9-in-fMP4 HLS path — **across segments**.
//!
//! The per-segment model (independent `ffmpeg -ss N*6 -t 6` runs + manual
//! `tfdt` surgery in `fmp4::process_segment`) is the prime suspect for the
//! reported "audio drifts ahead of video" playback bug. Structural checks
//! (tfdt == N·6·timescale, in `vp9_fmp4_hls.rs`) prove the *timeline math* but
//! NOT that a decoder plays audio and video in sync once the segments are
//! concatenated the way MSE does.
//!
//! This test generates a **known-sync source** — a 1-frame white flash and a
//! 40 ms 1 kHz beep at the SAME instant every second — pushes it through the
//! real segment handlers, concatenates `init.mp4 + 0.m4s + 1.m4s + …` (exactly
//! what hls.js feeds MSE), decodes the result, and measures the offset between
//! each video flash and its paired audio beep. In a correct pipeline every
//! offset is ~0 and stays flat across the 6 s segment boundaries; a segmentation
//! bug shows up as drift or a jump at t=6,12,18…
//!
//! `#[ignore]` + ffmpeg-gated like the other real-transcode suites.

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

const SECS: u32 = 30; // spans 5× 6 s segments → exercises 4 interior boundaries
                      // 23.976 fps (film/NTSC): 6 s = 143.856 frames, a NON-integer frames-per-
                      // segment, so segment cuts don't land on frame boundaries — the realistic case
                      // that a clean 30 fps (integer 180 frames/segment) source would hide.
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

/// Source with a synchronized marker each second: a 1-frame white flash on the
/// video and a 40 ms 1 kHz beep on the audio, both keyed off the same clock.
fn make_sync_clip(dir: &Path) -> std::path::PathBuf {
    let out = dir.join("sync.mkv");
    // Both markers are TIME-keyed (fps-independent) so they line up regardless
    // of frame rate: a ~30 ms white flash and a 40 ms 1 kHz beep in the first
    // slice of each second.
    let vf = "[0:v]drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t\\,1)\\,0.03)'[v];\
         [1:a]volume=enable='lt(mod(t\\,1)\\,0.04)':volume=1:eval=frame,\
         volume=enable='gte(mod(t\\,1)\\,0.04)':volume=0:eval=frame[a]"
        .to_string();
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
        .args(["-filter_complex", &vf])
        .args(["-map", "[v]", "-map", "[a]"])
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
    assert!(status.success(), "sync clip generation failed");
    out
}

/// Pull every numeric value that follows `label` in ffmpeg's log lines. Used to
/// read `black_end:`/`silence_end:` event times, which carry the real
/// presentation timestamp — no fps assumption, no amplitude thresholding.
fn parse_events(stderr: &[u8], label: &str) -> Vec<f64> {
    let s = String::from_utf8_lossy(stderr);
    let mut out = Vec::new();
    for line in s.lines() {
        let mut hay = line;
        while let Some(i) = hay.find(label) {
            let rest = hay[i + label.len()..].trim_start();
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                out.push(v);
            }
            hay = &hay[i + label.len()..];
        }
    }
    out
}

/// Video flash onset times: the white flash ends a black run, so each
/// `blackdetect` `black_end` is a flash onset at its true PTS.
fn flash_times(file: &Path) -> Vec<f64> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(file)
        .args([
            "-an",
            "-vf",
            "blackdetect=d=0.1:pix_th=0.10",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("ffmpeg blackdetect");
    parse_events(&out.stderr, "black_end:")
}

/// Audio beep onset times: each beep ends a silence run, so `silencedetect`
/// `silence_end` is a beep onset at its true PTS (catches boundary-attenuated
/// beeps that an RMS threshold would miss).
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
        .expect("ffmpeg silencedetect");
    parse_events(&out.stderr, "silence_end:")
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
            title: "sync".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(SECS as u64 * 1000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(400_000),
                video_codec: Some("h264".into()),
                audio_codec: Some("pcm".into()),
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

/// For each flash, find the NEAREST beep and return the signed offset
/// (beep − flash) when one exists within `tol`; `None` means the audio marker
/// was dropped — a desync signal that zip-pairing would hide by silently
/// shifting every later pair.
fn nearest_offsets(flashes: &[f64], beeps: &[f64], tol: f64) -> Vec<Option<f64>> {
    flashes
        .iter()
        .map(|&f| {
            beeps
                .iter()
                .copied()
                .map(|b| b - f)
                .min_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap())
                .filter(|d| d.abs() <= tol)
        })
        .collect()
}

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9 + libopus) on PATH"]
async fn vp9_av_sync_holds_across_segment_boundaries() {
    if !ffmpeg_ok() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let src = make_sync_clip(td.path());

    // Baseline: the SOURCE is perfectly synced — proves the harness measures 0.
    let src_flashes = flash_times(&src);
    let src_beeps = beep_times(&src);
    let src_off = nearest_offsets(&src_flashes, &src_beeps, 0.25);
    eprintln!(
        "source: {} flashes, {} beeps",
        src_flashes.len(),
        src_beeps.len()
    );
    assert!(
        src_flashes.len() >= (SECS as usize - 2),
        "source flash detection is broken: {} flashes",
        src_flashes.len()
    );
    assert!(
        src_off.iter().all(|o| o.is_some_and(|d| d.abs() < 0.05)),
        "harness sanity: source must measure in-sync, offsets(ms)={:?}",
        src_off
            .iter()
            .map(|o| o.map(|d| (d * 1000.0).round()))
            .collect::<Vec<_>>()
    );

    // Drive the real per-segment path and reassemble the stream MSE-style.
    let (state, token) = seed(src, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    let init = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/init.mp4?api_key={token}"))
            .to_request(),
    )
    .await;
    let mut stream = init.to_vec();
    let n_segments = SECS.div_ceil(6);
    for n in 0..n_segments {
        let seg = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!("/videos/42/vp9/{n}.m4s?api_key={token}"))
                .to_request(),
        )
        .await;
        assert!(!seg.is_empty(), "segment {n} came back empty");
        stream.extend_from_slice(&seg);
    }
    let concat = td.path().join("concat.mp4");
    std::fs::write(&concat, &stream).unwrap();
    // `AVSYNC_DUMP=/path` copies the reassembled stream out for manual probing.
    if let Ok(dst) = std::env::var("AVSYNC_DUMP") {
        std::fs::copy(&concat, &dst).ok();
        eprintln!("dumped reassembled stream to {dst}");
    }

    // Measure the reassembled transcode.

    let flashes = flash_times(&concat);
    let beeps = beep_times(&concat);
    let off = nearest_offsets(&flashes, &beeps, 0.5);
    eprintln!(
        "transcode: {} flashes, {} beeps",
        flashes.len(),
        beeps.len()
    );
    for (i, o) in off.iter().enumerate() {
        let t = flashes.get(i).copied().unwrap_or(0.0);
        let boundary = (t % 6.0) < 1.2 || (t % 6.0) > 4.8;
        let tag = if boundary {
            "  <- near 6s boundary"
        } else {
            ""
        };
        match o {
            Some(d) => eprintln!(
                "  event {i:2}  flash={t:.3}s  offset={:+.0}ms{tag}",
                d * 1000.0
            ),
            None => {
                eprintln!("  event {i:2}  flash={t:.3}s  offset=DROPPED (no beep within 0.5s){tag}")
            }
        }
    }

    assert!(
        flashes.len() >= (SECS as usize - 3),
        "lost flashes through the pipeline: {} of ~{SECS}",
        flashes.len()
    );
    // A dropped audio marker means a chunk of audio content vanished at a
    // boundary — the mechanism behind "audio runs ahead of video". This is the
    // load-bearing assertion.
    let dropped = off.iter().filter(|o| o.is_none()).count();
    assert_eq!(
        dropped,
        0,
        "audio markers dropped at segment boundaries (per-event above): {dropped} of {}",
        flashes.len()
    );
    // Among matched markers, a small constant codec-delay offset is fine; drift
    // or jumps above ~120 ms are audible desync.
    let max_off = off
        .iter()
        .filter_map(|o| o.map(f64::abs))
        .fold(0.0_f64, f64::max);
    assert!(
        max_off < 0.12,
        "A/V desync across segments: max offset {:.0}ms (per-event above)",
        max_off * 1000.0
    );
}
