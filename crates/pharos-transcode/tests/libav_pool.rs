//! Phase 2 — persistent libav worker pool. Drives real `transcode-worker`
//! subprocesses (built with `backend-lib`) over the socketpair, exercising
//! request/reply, worker reuse, and clean failure on malformed input
//! (no hang). The worker binary path comes from cargo's
//! `CARGO_BIN_EXE_transcode-worker`.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::protocol::WorkerError;
use pharos_transcode::worker::{LibavWorkerPool, PoolError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_transcode-worker"))
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn synth_fixture(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libopus",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

#[tokio::test]
async fn pool_probes_and_reuses_worker() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.webm");
    synth_fixture(&input);

    let pool = LibavWorkerPool::new(worker_bin(), 2).with_op_timeout(Duration::from_secs(30));

    // First probe spawns a worker; second reuses the resident one.
    let a = pool.probe(input.clone()).await.expect("probe a");
    let b = pool.probe(input.clone()).await.expect("probe b");
    assert_eq!(a.probe.video_codec.as_deref(), Some("vp9"));
    assert_eq!(a.probe.width, Some(320));
    // Same file → identical probe.
    assert_eq!(a.probe.video_codec, b.probe.video_codec);
    assert_eq!(a.probe.duration_ms, b.probe.duration_ms);

    // And an image op through the same pool.
    let out = dir.path().join("thumb.jpg");
    pool.extract_image(input, Some(500), 240, 3, out.clone())
        .await
        .expect("image");
    let bytes = std::fs::read(&out).unwrap();
    assert!(
        bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8,
        "not a JPEG"
    );
}

#[tokio::test]
async fn pool_reports_bad_input_then_recovers() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("garbage.webm");
    std::fs::write(&bad, b"this is not a media file").unwrap();

    let pool = LibavWorkerPool::new(worker_bin(), 1).with_op_timeout(Duration::from_secs(30));

    // Malformed input → clean BadInput, NOT a hang.
    let err = pool.probe(bad).await.expect_err("should fail");
    assert!(
        matches!(err, PoolError::Op(WorkerError::BadInput)),
        "expected Op(BadInput), got {err:?}"
    );

    // The worker survived a bad op (BadInput is reported, not a crash) and
    // is still usable: a subsequent good probe works.
    let good = dir.path().join("ok.webm");
    synth_fixture(&good);
    let info = pool.probe(good).await.expect("probe after bad input");
    assert_eq!(info.probe.video_codec.as_deref(), Some("vp9"));
}

#[tokio::test]
async fn pool_waveform_and_subtitle() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pool = LibavWorkerPool::new(worker_bin(), 2).with_op_timeout(Duration::from_secs(30));

    // Waveform over a tone.
    let wav = dir.path().join("tone.wav");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2:sample_rate=8000",
            "-ac",
            "1",
        ])
        .arg(&wav)
        .status()
        .unwrap();
    assert!(status.success());
    let bins = pool.waveform(wav, 1600, 8).await.expect("waveform");
    assert_eq!(bins.len(), 8);

    // SRT → WebVTT.
    let srt = dir.path().join("in.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nHello, world\n").unwrap();
    let vtt = dir.path().join("out.vtt");
    pool.srt_to_webvtt(srt, vtt.clone()).await.expect("srt");
    let text = std::fs::read_to_string(&vtt).unwrap();
    assert!(text.starts_with("WEBVTT"), "no WEBVTT header: {text:?}");
    assert!(text.contains("00:00:01.000 --> 00:00:02.000"));
}
