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
