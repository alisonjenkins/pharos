//! Spec 028 — `SubtitleSignals`: per-line audio signals and video keyframe
//! cuts from one demux pass.
//!
//! The fixture is 12 s of 5.1 audio over low-resolution video. The front-centre
//! channel carries a 220 Hz tone, loud in [1, 3] s and 20 dB down in [6, 8] s.
//! Front-left and front-right carry loud noise in [6, 8] s, so a mono downmix
//! would call that second line the loud one and only a front-centre read gets
//! it right. The video has a keyframe every 2 s plus forced ones at 5.3 s and
//! 9.1 s, which are the "cuts".
//!
//! "Reads the file once" has no observable counter: the op is one function
//! with one `format::input` and one `packets()` loop, so a second open would
//! be a visible second call in the source, not a runtime behaviour to probe.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::libav::subtitle_signals::subtitle_signals;
use pharos_transcode::subtitle_signals::ChannelPick;
use pharos_transcode::worker::LibavWorkerPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Channels in FL FR FC LFE BL BR order.
const AUDIO_EXPR: &str = "aevalsrc=exprs='0.5*(2*random(0)-1)*between(t,6,8)\
    |0.5*(2*random(1)-1)*between(t,6,8)\
    |0.5*sin(2*PI*220*t)*(between(t,1,3)+0.1*between(t,6,8))\
    |0|0|0':c=5.1:s=48000:d=12";

fn fixture(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=12:size=160x120:rate=24",
            "-f",
            "lavfi",
            "-i",
            AUDIO_EXPR,
            "-c:v",
            "mpeg4",
            "-g",
            "48",
            "-keyint_min",
            "48",
            "-sc_threshold",
            "0",
            "-force_key_frames",
            "5.3,9.1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "flac",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

fn near(list: &[u64], want: u64, tol: u64) -> bool {
    list.iter().any(|&t| t.abs_diff(want) <= tol)
}

const LINES: [(u64, u64); 2] = [(1000, 3000), (6000, 8000)];

#[test]
fn front_centre_loudness_pitch_and_cuts() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.mkv");
    fixture(&input);

    let s = subtitle_signals(&input, None, &LINES).unwrap();

    assert_eq!(s.channel, ChannelPick::FrontCentre);
    assert_eq!(s.lines.len(), 2);
    assert!(s.audio_stream.is_some());
    assert!(s.analysed_seconds > 8.0, "{}", s.analysed_seconds);

    let loud = s.lines[0].loudness_lufs.unwrap();
    let quiet = s.lines[1].loudness_lufs.unwrap();
    assert!(
        loud - quiet >= 15.0,
        "front-centre only: {loud} vs {quiet} LUFS (a downmix would invert this)"
    );
    let pitch = s.lines[0].pitch_hz.expect("a 220 Hz tone is voiced");
    assert!((pitch - 220.0).abs() <= 11.0, "pitch {pitch}");
    assert!(s.lines[0].voiced_ratio > 0.8, "{}", s.lines[0].voiced_ratio);
    // The loud line is a tone, so almost no energy above 3.4 kHz.
    assert!(s.lines[0].high_band_share.unwrap() < 0.05);

    // Keyframes: the 2 s GOP plus the two forced ones.
    for t in [0, 2000, 4000, 5300, 7300, 9100, 11100] {
        assert!(
            near(&s.keyframes_ms, t, 50),
            "keyframe near {t}: {:?}",
            s.keyframes_ms
        );
    }
    assert!(near(&s.cuts_ms, 5300, 50), "{:?}", s.cuts_ms);
    assert!(near(&s.cuts_ms, 9100, 50), "{:?}", s.cuts_ms);
    for regular in [0, 2000, 4000, 7300, 11100] {
        assert!(
            !near(&s.cuts_ms, regular, 50),
            "regular keyframe {regular} is not a cut: {:?}",
            s.cuts_ms
        );
    }
}

#[test]
fn a_window_past_the_audio_has_no_signals_and_a_bad_stream_is_refused() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.mkv");
    fixture(&input);

    let s = subtitle_signals(&input, None, &[(60_000, 62_000)]).unwrap();
    assert_eq!(s.lines[0].loudness_lufs, None);
    assert_eq!(s.lines[0].pitch_hz, None);

    // Stream 0 is the video.
    assert!(subtitle_signals(&input, Some(0), &LINES).is_err());
    assert!(subtitle_signals(&input, Some(99), &LINES).is_err());
    // The audio stream named explicitly is the one the default picks.
    let auto = subtitle_signals(&input, None, &LINES).unwrap();
    let named = subtitle_signals(&input, auto.audio_stream, &LINES).unwrap();
    assert_eq!(auto.lines, named.lines);
}

#[tokio::test]
async fn the_pool_carries_the_op_through_a_worker() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.mkv");
    fixture(&input);

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_transcode-worker"));
    let pool = LibavWorkerPool::new(bin, 1).with_op_timeout(Duration::from_secs(60));
    let s = pool
        .subtitle_signals(input, None, LINES.to_vec())
        .await
        .expect("subtitle_signals");
    assert_eq!(s.channel, ChannelPick::FrontCentre);
    assert!(s.lines[0].loudness_lufs.unwrap() - s.lines[1].loudness_lufs.unwrap() >= 15.0);
    assert!(near(&s.cuts_ms, 5300, 50));
}

fn counter(
    snap: &pharos_test_metrics::CumulativeSnapshotter,
    name: &str,
    labels: &[(&str, &str)],
) -> u64 {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .iter()
        .filter(|(k, ..)| {
            k.key().name() == name
                && labels
                    .iter()
                    .all(|(lk, lv)| k.key().labels().any(|l| l.key() == *lk && l.value() == *lv))
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(c) => *c,
            _ => 0,
        })
        .sum()
}

/// B914/V875: a track with more lines than one frame holds (79k on a live
/// One Piece episode) is analysed in chunks, each line keeps its own
/// signals, and the worker survives.
#[tokio::test]
async fn a_track_too_long_for_one_frame_is_analysed_in_chunks() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _guard = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.mkv");
    fixture(&input);
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_transcode-worker"));
    let pool = LibavWorkerPool::new(bin, 1).with_op_timeout(Duration::from_secs(120));

    let alone = pool
        .subtitle_signals(input.clone(), None, LINES.to_vec())
        .await
        .expect("two lines");
    // 70k distinct spans (more than one frame holds even deduplicated), each
    // twice, after the two reference lines.
    let distinct: Vec<(u64, u64)> = (0..70_000u64)
        .map(|i| {
            let start = i % 10_000;
            (start, start + 100 + (i / 10_000) * 500)
        })
        .collect();
    let mut windows = LINES.to_vec();
    windows.extend(&distinct);
    windows.extend(&distinct);
    assert!(distinct.len() * 16 > pharos_transcode::protocol::MAX_FRAME as usize);

    let s = pool
        .subtitle_signals(input, None, windows.clone())
        .await
        .expect("long track");
    assert_eq!(s.lines.len(), windows.len());
    assert_eq!(&s.lines[..2], &alone.lines[..]);
    assert_eq!(s.lines[2], s.lines[2 + distinct.len()]);
    assert_eq!(s.cuts_ms, alone.cuts_ms);
    assert_eq!(pool.idle_workers().await, 1);
    assert_eq!(
        counter(
            &snap,
            "pharos_libav_worker_lifecycle_total",
            &[("reason", "broken")]
        ),
        0
    );
    assert_eq!(counter(&snap, "pharos_libav_frame_too_large_total", &[]), 0);
}

/// B914: a request that cannot fit in a frame fails the op, is counted with
/// its op, and leaves the worker idle for the next op instead of killing it.
#[tokio::test]
async fn an_oversized_request_fails_the_op_and_keeps_the_worker() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _guard = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.mkv");
    fixture(&input);
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_transcode-worker"));
    let pool = LibavWorkerPool::new(bin, 1).with_op_timeout(Duration::from_secs(60));
    pool.subtitle_signals(input.clone(), None, LINES.to_vec())
        .await
        .expect("warm the worker");

    let huge: Vec<(u64, u64)> = (0..100_000u64).map(|i| (i, i + 1)).collect();
    let err = pool
        .fingerprint_multi(input.clone(), huge)
        .await
        .expect_err("oversized request");
    assert!(err.to_string().contains("frame too large"), "{err}");
    assert_eq!(pool.idle_workers().await, 1, "worker was discarded");
    assert_eq!(
        counter(
            &snap,
            "pharos_libav_frame_too_large_total",
            &[("direction", "request"), ("op", "fingerprint_multi")]
        ),
        1
    );

    pool.subtitle_signals(input, None, LINES.to_vec())
        .await
        .expect("same worker serves the next op");
    assert_eq!(
        counter(
            &snap,
            "pharos_libav_worker_lifecycle_total",
            &[("event", "spawned")]
        ),
        1
    );
    assert_eq!(
        counter(
            &snap,
            "pharos_libav_worker_lifecycle_total",
            &[("reason", "broken")]
        ),
        0
    );
}
