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
        matches!(err, PoolError::Op(WorkerError::BadInput(_))),
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

/// The integrity scan is only useful if it survives the crash boundary: it
/// runs in the worker precisely because its inputs are expected to be
/// malformed, so the verdict has to come back over the socketpair intact —
/// including the demuxer's own words, which are the part a bare boolean
/// would have dropped.
#[tokio::test]
async fn pool_scans_integrity_across_the_worker_boundary() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.mkv");
    let holed = dir.path().join("holed.mkv");
    synth_mkv(&good);
    std::fs::copy(&good, &holed).unwrap();
    punch_hole(&holed, 64 * 1024);

    let pool = LibavWorkerPool::new(worker_bin(), 1);

    let clean = pool.integrity(good).await.expect("scan good");
    assert!(
        !clean.is_damaged(),
        "an intact file must survive the round trip clean: {}",
        clean.summary()
    );
    assert!(
        clean.packets > 0,
        "no packets crossed the boundary: {clean:?}"
    );

    let damaged = pool.integrity(holed).await.expect("scan holed");
    assert!(
        damaged.is_damaged(),
        "the hole must be reported through the worker: {damaged:?}"
    );
    assert!(
        damaged.first_fault.is_some(),
        "the reason must survive the wire, not just the verdict: {damaged:?}"
    );
}

/// 20 s of H.264 in Matroska — long enough that a mid-file hole lands well
/// past the header, so the file still opens and still probes.
fn synth_mkv(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=20:size=320x240:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "25",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

/// Zero `len` bytes starting at the midpoint of the file.
fn punch_hole(path: &Path, len: usize) {
    use std::io::{Seek, SeekFrom, Write};
    let size = std::fs::metadata(path).expect("stat fixture").len();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open fixture");
    f.seek(SeekFrom::Start(size / 2)).expect("seek");
    f.write_all(&vec![0u8; len]).expect("write hole");
    f.sync_all().expect("sync");
}

#[tokio::test]
async fn pool_extracts_image_subtitles_across_the_worker_boundary() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = crate::dvd_subtitle::make_source(tmp.path());
    let out = tmp.path().join("out");
    let pool = LibavWorkerPool::new(worker_bin(), 1);

    let events = pool.subtitle_images(&src, 0, 0, &out).await.unwrap();

    assert_eq!(events, 2);
    assert!(out.join("manifest.json").is_file());
    assert!(out.join("0.png").is_file() && out.join("1.png").is_file());
}

/// Live `transcode-worker` children of this test process (nextest runs each
/// test in a process of its own).
fn live_workers() -> usize {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter(|stat| {
            let Some((head, rest)) = stat.rsplit_once(") ") else {
                return false;
            };
            // State Z is a reaped-but-unwaited zombie, not a live worker.
            let mut f = rest.split(' ');
            let state = f.next();
            head.ends_with("(transcode-worke")
                && f.next() == Some(me.as_str())
                && state != Some("Z")
        })
        .count()
}

async fn wait_for_live_workers(want: usize) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n = live_workers();
        if n == want || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn lifecycle_count(
    snap: &pharos_test_metrics::CumulativeSnapshotter,
    event: &str,
    reason: &str,
) -> u64 {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_libav_worker_lifecycle_total"
                && key
                    .labels()
                    .any(|l| l.key() == "event" && l.value() == event)
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(c) => *c,
            _ => 0,
        })
        .sum()
}

/// B913/V874: a worker idle for the idle timeout exits, and the next op
/// spawns a fresh one; both are counted.
#[tokio::test]
async fn an_idle_worker_is_shut_down_and_the_next_op_respawns_it() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _guard = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.webm");
    synth_fixture(&input);
    let pool =
        LibavWorkerPool::new(worker_bin(), 2).with_idle_timeout(Some(Duration::from_millis(300)));

    pool.probe(input.clone()).await.expect("first probe");
    assert_eq!(pool.idle_workers().await, 1);
    assert_eq!(wait_for_live_workers(1).await, 1);

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        pool.idle_workers().await,
        0,
        "idle worker was not shut down"
    );
    assert_eq!(
        wait_for_live_workers(0).await,
        0,
        "worker process still running"
    );
    assert_eq!(lifecycle_count(&snap, "exited", "idle_timeout"), 1);

    pool.probe(input).await.expect("probe after the reap");
    assert_eq!(pool.idle_workers().await, 1);
    assert_eq!(lifecycle_count(&snap, "spawned", "no_idle_worker"), 2);
}

/// A worker in use within the timeout is kept; without a timeout none exits.
#[tokio::test]
async fn busy_or_unbounded_workers_stay_resident() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.webm");
    synth_fixture(&input);

    let busy =
        LibavWorkerPool::new(worker_bin(), 1).with_idle_timeout(Some(Duration::from_millis(400)));
    for _ in 0..4 {
        busy.probe(input.clone()).await.expect("probe");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert_eq!(busy.idle_workers().await, 1, "worker reaped while in use");

    let resident = LibavWorkerPool::new(worker_bin(), 1).with_idle_timeout(None);
    resident.probe(input).await.expect("probe");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(resident.idle_workers().await, 1);
}
