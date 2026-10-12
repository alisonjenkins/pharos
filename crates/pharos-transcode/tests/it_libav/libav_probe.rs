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

/// Synthesise a 1s MP4 carrying the given `-metadata` pairs.
fn synth_tagged_mp4(path: &Path, tags: &[&str]) {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc=duration=1:size=160x120:rate=10",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
    ]);
    for t in tags {
        cmd.args(["-metadata", t]);
    }
    let status = cmd.arg(path).status().expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

/// B169 — the deployed backend is libav, and its tag reader aliased the
/// container's `creation_time` into `date`, so it poisoned BOTH the year and
/// the premiere date. On the live library that dated 139 films by when they
/// were copied in: "300" and "300 - Rise of an Empire" both 2026-07-19,
/// "Avatar" 2026-07-20, "Apocalypse Now" 2025-05-06.
///
/// Uses a REAL fixture rather than a hand-built tag map, because the defect
/// was in which container keys are consulted — the thing a mocked map assumes
/// rather than tests. ffmpeg stamps `creation_time` into an MP4 by itself, so
/// the trap is present without asking for it.
#[test]
fn a_containers_mux_time_is_not_a_release_date() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("muxed.mp4");
    synth_tagged_mp4(&input, &["creation_time=2026-07-29T11:04:12.000000Z"]);

    let p = pharos_transcode::libav::probe::probe(&input)
        .expect("probe ok")
        .probe;
    assert_eq!(
        p.release_date, None,
        "the mux timestamp must not become a release date"
    );
    assert_eq!(
        p.year, None,
        "nor a production year — that is how Lara Croft (2003) became 2026"
    );
}

/// The other half: dropping the mux-time fallback must not cost the files
/// that carry a genuine release date.
#[test]
fn a_real_date_tag_is_still_read() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("dated.mp4");
    synth_tagged_mp4(
        &input,
        &[
            "date=2003-07-21",
            "creation_time=2026-07-29T11:04:12.000000Z",
        ],
    );

    let p = pharos_transcode::libav::probe::probe(&input)
        .expect("probe ok")
        .probe;
    assert_eq!(
        p.year,
        Some(2003),
        "a real date tag still supplies the year"
    );
    assert!(
        p.release_date
            .as_deref()
            .is_some_and(|d| d.starts_with("2003-07-21")),
        "release_date = {:?}",
        p.release_date
    );
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
