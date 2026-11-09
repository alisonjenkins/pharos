//! The black-bar check on a real file: a 4:3 picture padded into a 16:9
//! frame is found by seeking and decoding keyframes through libav, which the
//! synthetic-plane unit tests cannot show (stride, pixel format, seek).
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::picture_area::PictureVerdict;
use std::process::Command;

fn make_source(dir: &std::path::Path, filter: &str) -> std::path::PathBuf {
    let out = dir.join("src.mkv");
    let st = Command::new("ffmpeg")
        .args(["-v", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=30:size=480x270:rate=10",
        ])
        .args(["-vf", filter])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "10", "-y"])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg (run inside the devShell)");
    assert!(st.success(), "fixture synth failed");
    out
}

#[test]
fn a_pillarboxed_file_reports_its_picture_area() {
    let dir = tempfile::tempdir().unwrap();
    let src = make_source(dir.path(), "scale=480:360,pad=640:360:80:0:black");

    // The scan's runtime, as the sweep passes it; the second check reads the
    // container's own and must agree.
    let a = pharos_transcode::libav::picture_area::picture_area(&src, 30_000).expect("check");
    let own = pharos_transcode::libav::picture_area::picture_area(&src, 0).expect("check");
    assert_eq!(
        a, own,
        "the scan's runtime and the container's sample alike"
    );

    assert_eq!((a.width, a.height), (640, 360));
    assert_eq!(a.sampled_frames, 7, "one keyframe per sample point");
    match a.verdict {
        PictureVerdict::Bars(area) => {
            assert!(area.x.abs_diff(80) <= 4, "x: {area:?}");
            assert!(area.width.abs_diff(480) <= 8, "width: {area:?}");
            assert_eq!((area.y, area.height), (0, 360), "full height: {area:?}");
        }
        other => panic!("expected bars, got {other:?} ({a:?})"),
    }
}

#[test]
fn a_file_that_cannot_be_read_is_retried_not_stored_as_undecodable() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone.mkv");

    let err =
        pharos_transcode::libav::picture_area::picture_area(&gone, 30_000).expect_err("no file");

    assert!(
        matches!(err, pharos_transcode::libav::frames::FrameError::Other(_)),
        "a missing file says nothing about its picture: {err:?}"
    );
}

#[test]
fn a_file_with_no_bars_reports_none() {
    let dir = tempfile::tempdir().unwrap();
    let src = make_source(dir.path(), "scale=640:360");

    let a = pharos_transcode::libav::picture_area::picture_area(&src, 0).expect("check");

    assert_eq!(a.verdict, PictureVerdict::NoBars, "{a:?}");
}
