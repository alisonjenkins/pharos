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
        decode_offload: pharos_transcode::DecodeOffload::Allowed,
        vaapi_pipeline: pharos_transcode::VaapiPipeline::Hybrid,
        job_id: JobId(1),
        input: input.clone(),
        opts: TranscodeOptions {
            source_video_codec: None,
            source_video_kind: Default::default(),
            source_frame_rate: None,
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
            filler: None,
            video_3d_format: None,
            stereo_output: Default::default(),
        },
        device: DeviceId::Cpu,
        sink: OutputSink::FileDirect { path: out.clone() },
        filler: None,
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
        decode_offload: pharos_transcode::DecodeOffload::Allowed,
        vaapi_pipeline: pharos_transcode::VaapiPipeline::Hybrid,
        job_id: JobId(2),
        input: dir.path().join("does-not-exist.mkv"),
        opts: TranscodeOptions {
            source_video_codec: None,
            source_video_kind: Default::default(),
            source_frame_rate: None,
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
            filler: None,
            video_3d_format: None,
            stereo_output: Default::default(),
        },
        device: DeviceId::Cpu,
        sink: OutputSink::FileDirect { path: out.clone() },
        filler: None,
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
        decode_offload: pharos_transcode::DecodeOffload::Allowed,
        vaapi_pipeline: pharos_transcode::VaapiPipeline::Hybrid,
        job_id: JobId(9),
        input,
        opts: TranscodeOptions {
            source_video_codec: None,
            source_video_kind: Default::default(),
            source_frame_rate: None,
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
            filler: None,
            video_3d_format: None,
            stereo_output: Default::default(),
        },
        device: DeviceId::Cpu,
        sink: OutputSink::Stdout,
        filler: None,
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

fn timing_spec(input: &Path, out: &Path) -> JobSpec {
    JobSpec {
        decode_offload: pharos_transcode::DecodeOffload::Allowed,
        vaapi_pipeline: pharos_transcode::VaapiPipeline::Hybrid,
        job_id: JobId(7),
        input: input.to_path_buf(),
        opts: TranscodeOptions {
            source_video_codec: None,
            source_video_kind: Default::default(),
            source_frame_rate: None,
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
            video_3d_format: None,
            stereo_output: Default::default(),
            filler: None,
        },
        device: DeviceId::Cpu,
        sink: OutputSink::FileDirect {
            path: out.to_path_buf(),
        },
        filler: None,
    }
}

/// Run one real ffmpeg job through a real worker and hand back its timing.
async fn run_for_timing(
    mode: pharos_transcode::stage_timing::StageTimingMode,
) -> Option<pharos_transcode::stage_timing::StageTiming> {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.webm");
    synth_fixture(&input).await;
    let out = dir.path().join("0.tmp");
    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN).with_stage_timing(mode);
    let mut worker = spawner
        .spawn(pharos_transcode::protocol::WorkerId(2))
        .await
        .expect("spawn worker");
    let res = tokio::time::timeout(
        Duration::from_secs(60),
        worker.run(timing_spec(&input, &out)),
    )
    .await
    .expect("worker run timed out");
    assert!(matches!(res, WorkerRunResult::Done { .. }), "got {res:?}");
    worker.take_stage_timing()
}

#[tokio::test]
async fn basic_mode_records_spawn_total_and_frames_from_a_real_encode() {
    use pharos_transcode::stage_timing::{Stage, StageTimingMode};
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let t = run_for_timing(StageTimingMode::Basic)
        .await
        .expect("basic mode reports timing");
    let stages: Vec<Stage> = t.stages().into_iter().map(|(s, _)| s).collect();
    assert!(stages.contains(&Stage::Spawn) && stages.contains(&Stage::Total));
    assert!(t.spawn_secs > 0.0 && t.total_secs > t.spawn_secs, "{t:?}");
    if let Some(ff) = t.first_frame_secs {
        assert!(ff <= t.total_secs, "{t:?}");
    }
    let p = t.progress.expect("sidecar progress");
    assert!(p.ended, "{p:?}");
    assert_eq!(p.frames, 20, "2 s at 10 fps");
    assert!(t.effective_fps().unwrap_or(0.0) > 0.0);
    assert!(t.bench.is_none(), "benchmark is a diagnostic-mode cost");
}

#[tokio::test]
async fn detailed_mode_adds_decode_encode_and_other_stages() {
    use pharos_transcode::stage_timing::{Stage, StageTimingMode};
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let t = run_for_timing(StageTimingMode::Detailed)
        .await
        .expect("detailed mode reports timing");
    let get = |want: Stage| t.stages().into_iter().find(|(s, _)| *s == want);
    for stage in [Stage::DecodeVideo, Stage::EncodeVideo, Stage::Other] {
        let (_, secs) = get(stage).unwrap_or_else(|| panic!("{stage:?} missing: {t:?}"));
        assert!(secs > 0.0, "{stage:?} should be positive: {t:?}");
    }
    let b = t.bench.as_ref().expect("bench report");
    assert!(b.rtime.is_some() && b.utime.is_some(), "{b:?}");
}

#[tokio::test]
async fn off_mode_reports_nothing_and_still_encodes() {
    use pharos_transcode::stage_timing::StageTimingMode;
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    assert!(run_for_timing(StageTimingMode::Off).await.is_none());
}

/// Run `spec` through a real worker in basic mode, after `before` has had a
/// chance to plant files next to the output.
async fn run_spec_basic(
    spec: JobSpec,
    out: &Path,
) -> Option<pharos_transcode::stage_timing::StageTiming> {
    let spawner = ProcSpawner::with_worker_bin(WORKER_BIN)
        .with_stage_timing(pharos_transcode::stage_timing::StageTimingMode::Basic);
    let mut worker = spawner
        .spawn(pharos_transcode::protocol::WorkerId(3))
        .await
        .expect("spawn worker");
    let res = tokio::time::timeout(Duration::from_secs(120), worker.run(spec))
        .await
        .expect("worker run timed out");
    assert!(matches!(res, WorkerRunResult::Done { .. }), "got {res:?}");
    assert!(out.exists());
    worker.take_stage_timing()
}

/// An audio-only job never writes a `frame=` line, so the first-frame poll used
/// to run for the whole job. Its first output is the timeline moving, and that
/// must end the poll and be recorded.
#[tokio::test]
async fn an_audio_only_job_records_a_first_output() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("tone.flac");
    // Long enough that the encode outlives several stats periods.
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("sine=frequency=440:duration=300")
        .arg(&input)
        .status()
        .await
        .expect("spawn ffmpeg");
    assert!(status.success());
    let out = dir.path().join("0.tmp");
    let mut spec = timing_spec(&input, &out);
    spec.opts.video = None;
    spec.opts.container = Container::Adts;
    let t = run_spec_basic(spec, &out).await.expect("timing");
    assert!(t.progress.is_some_and(|p| p.frames == 0), "{t:?}");
    let first = t
        .first_frame_secs
        .expect("first output recorded for audio-only");
    assert!(first < t.total_secs, "{t:?}");
}

/// A retry reuses the output path. A sidecar left by the previous attempt must
/// not be mistaken for this attempt's first frame.
#[tokio::test]
async fn a_stale_progress_sidecar_is_not_read_as_a_first_frame() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.webm");
    synth_fixture(&input).await;
    let out = dir.path().join("0.tmp");
    std::fs::write(
        pharos_transcode::progress_sidecar_path(&out),
        "frame=999\nout_time_us=99000000\nprogress=end\n",
    )
    .unwrap();
    let t = run_spec_basic(timing_spec(&input, &out), &out)
        .await
        .expect("timing");
    assert_eq!(
        t.progress.map(|p| p.frames),
        Some(20),
        "final report is this attempt's"
    );
    // ffmpeg cannot produce a frame within 20 ms of being spawned.
    assert!(
        t.first_frame_secs.is_none_or(|f| f >= 0.02),
        "stale sidecar read as first frame: {t:?}"
    );
}
