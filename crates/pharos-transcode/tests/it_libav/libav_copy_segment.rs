//! The H.264 copy surface against real media (spec 041, V1431–V1434): a
//! source cut on its own keyframes and copied into fMP4 must carry exactly the
//! source's video packets, in order, and decode to the same pictures.
//!
//! The fixtures are synthesised with irregular forced keyframes and B-frames,
//! the shape that broke CLI `-c copy` cutting (V30).
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::copy_fmp4;
use pharos_transcode::copy_surface::{CopyIndexVerdict, CopyRefusal, CopySource};
use pharos_transcode::libav::copy_segment;
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

/// Keyframe times, all on the 25 fps frame grid, spaced so the planner has to
/// group some and keep others apart.
const KEYFRAMES: &str = "0,2.52,7.12,13.32,14,21.92,28,33.72,41,47.52";

fn synth(path: &Path, open_gop: bool) {
    let x264 = if open_gop {
        "scenecut=0:open_gop=1:keyint=600:min-keyint=1"
    } else {
        "scenecut=0:open_gop=0:keyint=600:min-keyint=1"
    };
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=50:size=320x240:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-profile:v",
            "high",
            "-bf",
            "3",
            "-force_key_frames",
            KEYFRAMES,
            "-x264-params",
            x264,
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

fn copyable(src: &Path) -> CopySource {
    match copy_segment::index(src).expect("index") {
        CopyIndexVerdict::Copyable(s) => *s,
        CopyIndexVerdict::Refused(r) => panic!("refused: {r}"),
    }
}

/// init + every segment, concatenated, as one fragmented MP4.
fn copy_all(src: &Path, source: &CopySource, dir: &Path) -> (std::path::PathBuf, Vec<Vec<u8>>) {
    let mut whole = copy_fmp4::init_segment(&source.track).expect("init");
    let mut segments = Vec::new();
    let n = source.plan.segments.len();
    for (i, seg) in source.plan.segments.iter().enumerate() {
        let out = dir.join(format!("{i}.m4s"));
        let report = copy_segment::copy_segment(src, i, *seg, i + 1 == n, &out).expect("copy");
        let bytes = std::fs::read(&out).expect("read segment");
        assert_eq!(report.bytes, bytes.len() as u64);
        assert!(report.packets > 0);
        whole.extend_from_slice(&bytes);
        segments.push(bytes);
    }
    let path = dir.join("copied.mp4");
    std::fs::write(&path, &whole).expect("write copied");
    (path, segments)
}

/// One line per video packet or frame: `pts,size,hash` from ffmpeg's
/// framemd5 muxer. `copy` hashes the packets; otherwise the decoded pictures.
fn framemd5(path: &Path, copy: bool) -> Vec<String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0"]);
    if copy {
        cmd.args(["-c", "copy"]);
    }
    let out = cmd
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("spawn ffmpeg framemd5");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            // stream, dts, pts, duration, size, hash
            format!("{},{}", f[4], f[5])
        })
        .collect()
}

/// Each video packet's presentation time in microseconds, decode order,
/// measured from the earliest one, so a container start offset cancels out.
fn packet_pts_us(path: &Path) -> Vec<i64> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "framemd5", "-"])
        .output()
        .expect("spawn ffmpeg framemd5");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    let tb = text
        .lines()
        .find_map(|l| l.strip_prefix("#tb 0: "))
        .expect("time base line");
    let (num, den) = tb.split_once('/').expect("num/den");
    let (num, den): (i128, i128) = (num.trim().parse().unwrap(), den.trim().parse().unwrap());
    let pts: Vec<i64> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let pts: i128 = f[2].parse().unwrap();
            i64::try_from(pts * num * 1_000_000 / den).unwrap()
        })
        .collect();
    let first = *pts.iter().min().expect("packets");
    pts.into_iter().map(|p| p - first).collect()
}

#[test]
fn copied_segments_carry_exactly_the_source_packets() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.mkv");
    synth(&src, false);

    let source = copyable(&src);
    assert!(source.plan.segments.len() >= 5, "{:?}", source.plan);
    let (copied, _) = copy_all(&src, &source, dir.path());

    let packets = framemd5(&src, true);
    assert_eq!(packets.len(), 50 * 25, "every source frame is a packet");
    assert_eq!(framemd5(&copied, true), packets, "packets differ");
    assert_eq!(
        packet_pts_us(&copied),
        packet_pts_us(&src),
        "presentation times differ"
    );
    assert_eq!(
        framemd5(&copied, false),
        framemd5(&src, false),
        "pictures differ"
    );
}

#[test]
fn each_segment_starts_where_the_previous_one_ended() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.mkv");
    synth(&src, false);
    let source = copyable(&src);
    let (_, segments) = copy_all(&src, &source, dir.path());
    let init = copy_fmp4::init_segment(&source.track).expect("init");
    let defaults = pharos_hls::fmp4::track_defaults(&init).expect("trex");

    let mut expected = source.plan.segments[0].start;
    for (i, bytes) in segments.iter().enumerate() {
        let samples = pharos_hls::fmp4::samples(bytes, &defaults).expect("parse segment");
        let tfdt = pharos_hls::fmp4::decode_times(bytes).expect("tfdt");
        assert_eq!(tfdt.len(), 1, "segment {i}");
        assert_eq!(
            i64::try_from(tfdt[0].1).unwrap(),
            expected,
            "segment {i} tfdt"
        );
        assert_eq!(
            expected, source.plan.segments[i].start,
            "segment {i} starts on its planned keyframe"
        );
        let total: u64 = samples.iter().map(|s| u64::from(s.duration)).sum();
        expected += i64::try_from(total).unwrap();
    }
}

#[test]
fn an_open_gop_source_is_refused() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("open.mkv");
    synth(&src, true);
    match copy_segment::index(&src).expect("index") {
        CopyIndexVerdict::Refused(CopyRefusal::Segment { detail, .. }) => {
            // x264's open-GOP keyframes are recovery-point I pictures, so the
            // IDR check meets them before the presentation-time check does.
            assert!(detail.contains("not an IDR picture"), "{detail}");
        }
        other => panic!("an open-GOP source must be refused on a segment: {other:?}"),
    }
}

#[test]
fn an_mp4_source_is_refused_on_its_container() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.mp4");
    synth(&src, false);
    match copy_segment::index(&src).expect("index") {
        CopyIndexVerdict::Refused(CopyRefusal::Container(name)) => {
            assert!(name.contains("mp4"), "{name}");
        }
        other => panic!("an MP4 source must be refused on its container: {other:?}"),
    }
}

#[test]
fn the_copied_track_is_the_first_video_track_the_probe_reports() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("two-tracks.mkv");
    // Track 0: a small, non-default MPEG-4 Part 2 track. Track 1: a larger,
    // default-marked H.264 track, the one libav's "best stream" ranks first.
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=10:size=160x120:rate=25",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=10:size=320x240:rate=25",
            "-map",
            "0:v",
            "-map",
            "1:v",
            "-c:v:0",
            "mpeg4",
            "-c:v:1",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-disposition:v:0",
            "0",
            "-disposition:v:1",
            "default",
        ])
        .arg(&src)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
    match copy_segment::index(&src).expect("index") {
        CopyIndexVerdict::Refused(CopyRefusal::Codec(codec)) => {
            assert!(codec.to_lowercase().contains("mpeg4"), "{codec}");
        }
        other => panic!("the first video track is MPEG-4, so copy must refuse it: {other:?}"),
    }
}
