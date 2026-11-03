//! A real `dvd_subtitle` source for the image-subtitle tests (spec 019).
//! ffmpeg cannot encode a bitmap subtitle from text, so it is built with
//! dvdauthor's `spumux` from two solid PNG cues, then remuxed to Matroska
//! with the subtitle copied, as the `09-test-dvd-subtitle.mkv` fixture is.
#![allow(dead_code)]

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
pub fn make_source(dir: &Path) -> PathBuf {
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
