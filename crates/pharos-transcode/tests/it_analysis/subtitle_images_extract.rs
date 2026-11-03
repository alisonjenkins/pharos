//! Spec 019 — image-subtitle extraction on a real `dvd_subtitle` track.
//! ffmpeg cannot encode a bitmap subtitle from text, so the source is built
//! with dvdauthor's `spumux` from two solid PNG cues, then remuxed to
//! Matroska with the subtitle copied, as the `09-test-dvd-subtitle.mkv`
//! fixture is.
#![cfg(all(unix, feature = "backend-lib"))]

use pharos_transcode::libav::subtitle_images::{extract_subtitle_images, MANIFEST_FILE};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn cue(dir: &Path, name: &str, colour: &str, size: &str) {
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=c={colour}:s={size},format=rgba"))
        .args(["-frames:v", "1"])
        .arg(dir.join(name)));
}

/// A white 240x48 cue at (240,400) from 0.5 to 2.5 s and a yellow 160x48
/// cue at (280,400) from 4 to 6 s, on a 720x480 NTSC programme.
fn make_source(dir: &Path) -> PathBuf {
    cue(dir, "cue1.png", "white", "240x48");
    cue(dir, "cue2.png", "yellow", "160x48");
    let base = dir.join("base.mpg");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg("testsrc=duration=8:size=720x480:rate=30000/1001")
        .args(["-target", "ntsc-dvd", "-an"])
        .arg(&base));
    std::fs::write(
        dir.join("subs.xml"),
        r#"<subpictures><stream>
<spu start="00:00:00.50" end="00:00:02.50" image="cue1.png" xoffset="240" yoffset="400"/>
<spu start="00:00:04.00" end="00:00:06.00" image="cue2.png" xoffset="280" yoffset="400"/>
</stream></subpictures>"#,
    )
    .unwrap();
    let subbed = dir.join("subbed.mpg");
    let out = Command::new("spumux")
        .current_dir(dir)
        .args(["-m", "dvd", "subs.xml"])
        .stdin(std::fs::File::open(&base).unwrap())
        .stdout(std::fs::File::create(&subbed).unwrap())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "spumux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mkv = dir.join("src.mkv");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&subbed)
        .args(["-map", "0:v", "-map", "0:s", "-c", "copy"])
        .arg(&mkv));
    mkv
}

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
