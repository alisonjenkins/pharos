//! Phase 1 — in-process libav probe parity. Synthesises a known
//! VP9/Opus WebM fixture via `ffmpeg -f lavfi` (skips cleanly when ffmpeg
//! isn't on PATH) and asserts the in-process `libav::probe` produces the
//! same `ProbeInfo` fields the spawn-path `FfmpegProber` does.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

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
    // 2s VP9/Opus WebM, 320x240 @ 10fps — matches worker_ipc.rs.
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

#[test]
fn probe_matches_known_fixture() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("fixture.webm");
    synth_fixture(&input);

    let info = pharos_transcode::libav::probe::probe(&input).expect("probe ok");
    let p = &info.probe;

    assert_eq!(p.video_codec.as_deref(), Some("vp9"), "video codec");
    assert_eq!(p.audio_codec.as_deref(), Some("opus"), "audio codec");
    assert_eq!(p.width, Some(320), "width");
    assert_eq!(p.height, Some(240), "height");
    assert_eq!(p.frame_rate_mille, Some(10_000), "fps×1000");
    assert_eq!(p.pixel_format.as_deref(), Some("yuv420p"), "pix_fmt");
    assert_eq!(p.audio_channels, Some(1), "channels (mono sine)");
    assert_eq!(p.sample_rate, Some(48_000), "opus sample rate");
    assert!(
        p.container.as_deref().is_some_and(|c| c.contains("webm")),
        "container = {:?}",
        p.container
    );
    // 2s ± container slack.
    let dur = p.duration_ms.expect("duration");
    assert!((1_800..=2_200).contains(&dur), "duration_ms = {dur}");
    assert_eq!(p.audio_tracks.len(), 1, "one audio track");
    assert_eq!(p.subtitle_tracks.len(), 0, "no subtitle tracks");
}

#[test]
fn probe_rejects_garbage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = dir.path().join("garbage.webm");
    std::fs::write(&bad, b"not a media file at all").expect("write garbage");

    let err = pharos_transcode::libav::probe::probe(&bad).expect_err("should fail");
    assert!(
        matches!(err, pharos_transcode::libav::probe::ProbeError::BadInput(_)),
        "expected BadInput, got {err:?}"
    );
}
