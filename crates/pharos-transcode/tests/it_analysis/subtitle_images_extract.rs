//! Spec 019 — image-subtitle extraction on a real `dvd_subtitle` track
//! (`dvd_subtitle::make_source`).
#![cfg(all(unix, feature = "backend-lib"))]

use crate::dvd_subtitle::make_source;
use pharos_transcode::libav::subtitle_images::{extract_subtitle_images, MANIFEST_FILE};
use std::path::Path;
use std::process::Command;

/// The RGBA of a PNG's centre pixel, decoded by ffmpeg.
fn centre_rgba(png: &Path) -> [u8; 4] {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(png)
        .args(["-vf", "crop=1:1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout[..4].try_into().unwrap()
}

#[test]
fn a_dvd_subtitle_track_extracts_to_timed_pngs_on_its_canvas() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_source(tmp.path());
    let out = tmp.path().join("out");

    let m = extract_subtitle_images(&src, 0, &out).unwrap();

    assert!(m.complete);
    assert_eq!((m.width, m.height), (720, 480), "the DVD canvas");
    assert_eq!(m.events.len(), 2, "{m:?}");
    let (a, b) = (&m.events[0], &m.events[1]);
    // spumux snaps to the frame grid and the mux to the 90 kHz clock.
    let near = |got: u64, want: u64| got.abs_diff(want) <= 100;
    assert!(near(a.start_ms, 500) && near(a.end_ms, 2_500), "{a:?}");
    assert!(near(b.start_ms, 4_000) && near(b.end_ms, 6_000), "{b:?}");
    assert_eq!((a.x, a.y, a.width, a.height), (240, 400, 240, 48));
    assert_eq!((b.x, b.y, b.width, b.height), (280, 400, 160, 48));

    let png = std::fs::read(out.join(&a.file)).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    // The cues decode to opaque pixels. (spumux keeps its palette in the
    // IFO, not the stream, so the decoder's default palette colours them;
    // palette mapping itself is unit-tested beside `apply_palette`.)
    assert_eq!(centre_rgba(&out.join(&a.file))[3], 255);
    assert_eq!(centre_rgba(&out.join(&b.file))[3], 255);
    let on_disk: pharos_transcode::libav::subtitle_images::ImageSubManifest =
        serde_json::from_slice(&std::fs::read(out.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(on_disk, m);
}

#[test]
fn a_missing_subtitle_stream_is_bad_input() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_source(tmp.path());
    let err = extract_subtitle_images(&src, 3, &tmp.path().join("out")).unwrap_err();
    assert!(err.to_string().contains("no subtitle stream s:3"), "{err}");
}
