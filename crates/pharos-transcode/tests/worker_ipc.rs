//! Step-4 integration: real `transcode-worker` subprocess over a
//! socketpair, driven through the scheduler's `WorkerSpawner`/`Worker`
//! traits. Uses `CARGO_BIN_EXE_transcode-worker` so cargo builds + points
//! us at the worker binary. The handshake test needs no ffmpeg; the
//! encode test synthesises a fixture via `ffmpeg -f lavfi` and skips
//! cleanly when ffmpeg isn't on PATH.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use pharos_transcode::options::{AudioCodec, Container, TranscodeOptions, VideoCodec};
use pharos_transcode::protocol::{DeviceId, JobId, JobSpec, OutputSink};
use pharos_transcode::scheduler::{WorkerRunResult, WorkerSpawner};
use pharos_transcode::worker::ProcSpawner;
use std::path::Path;
use std::time::Duration;

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_transcode-worker");

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn worker_handshakes_over_socketpair() {
    // No ffmpeg needed — the worker sends Hello regardless.
    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN);
    let worker = spawner
        .spawn(pharos_transcode::protocol::WorkerId(0))
        .await
        .expect("spawn worker");
    // Downcast not exposed; just assert the worker exists + has an id.
    assert_eq!(worker.id(), pharos_transcode::protocol::WorkerId(0));
    // Dropping the worker kills the child (kill_on_drop).
}

#[tokio::test]
async fn worker_transcodes_file_direct() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.webm");
    synth_fixture(&input).await;

    let out = dir.path().join("0.tmp");
    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN);
    let mut worker = spawner
        .spawn(pharos_transcode::protocol::WorkerId(1))
        .await
        .expect("spawn worker");

    let spec = JobSpec {
        job_id: JobId(1),
        input: input.clone(),
        opts: TranscodeOptions {
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
        },
        device: DeviceId::Cpu,
        sink: OutputSink::FileDirect { path: out.clone() },
    };

    let res = tokio::time::timeout(Duration::from_secs(60), worker.run(spec))
        .await
        .expect("worker run timed out");
    match res {
        WorkerRunResult::Done { out_bytes } => {
            assert!(out_bytes > 0, "output should be non-empty");
            assert!(out.exists(), "output file should exist");
            let meta = std::fs::metadata(&out).unwrap();
            assert_eq!(meta.len(), out_bytes);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn worker_reports_bad_input() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("0.tmp");
    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN);
    let mut worker = spawner
        .spawn(pharos_transcode::protocol::WorkerId(2))
        .await
        .expect("spawn worker");

    let spec = JobSpec {
        job_id: JobId(2),
        input: dir.path().join("does-not-exist.mkv"),
        opts: TranscodeOptions {
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
        },
        device: DeviceId::Cpu,
        sink: OutputSink::FileDirect { path: out.clone() },
    };

    let res = tokio::time::timeout(Duration::from_secs(30), worker.run(spec))
        .await
        .expect("worker run timed out");
    // A missing input file is a non-recoverable failure, not a hang/crash.
    assert!(
        matches!(res, WorkerRunResult::Failed(_)),
        "expected Failed, got {res:?}"
    );
}

#[tokio::test]
async fn worker_streams_live_to_stdout() {
    use futures_util::StreamExt;
    use pharos_transcode::protocol::OutputSink;
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.webm");
    synth_fixture(&input).await;

    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN);
    let spec = JobSpec {
        job_id: JobId(9),
        input,
        opts: TranscodeOptions {
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
        },
        device: DeviceId::Cpu,
        sink: OutputSink::Stdout,
    };

    let mut stream = spawner
        .spawn_streaming(spec)
        .await
        .expect("spawn streaming");
    let mut total = 0usize;
    let mut first_byte = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Ok(Some(chunk)) => {
                let b = chunk.expect("stream chunk");
                if first_byte.is_none() && !b.is_empty() {
                    first_byte = Some(b[0]);
                }
                total += b.len();
            }
            Ok(None) => break,
            Err(_) => panic!("live stream timed out"),
        }
    }
    assert!(total > 0, "live stream produced no bytes");
    // MPEG-TS packets begin with the 0x47 sync byte.
    assert_eq!(first_byte, Some(0x47), "expected MPEG-TS sync byte");
}

async fn synth_fixture(path: &Path) {
    // 2s VP9/Opus WebM — small + FOSS-decodable.
    let status = tokio::process::Command::new("ffmpeg")
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
        .await
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}
