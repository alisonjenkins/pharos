//! Behavioural gate for per-eye subtitle burn-in on packed 3D (V234).
//!
//! The argv tests prove the graph SHAPE; only ffmpeg can say it accepts the
//! graph and puts the subtitle where intended (V30). Each case drives the REAL
//! production argv over a tiny black lavfi clip laid out as one packed format
//! and a generated `.srt` whose cue is active 29-31 s, then inspects the
//! output: both eyes must carry the subtitle, and the frame size is unchanged.
//! A control run with `Mono2D` shows the same probe sees one eye only when
//! the subtitle is drawn once.
//!
//! ffmpeg-gated + `#[ignore]` like the other real-transcode suites. Run:
//!   nix develop --command cargo nextest run -p pharos-transcode --run-ignored all subtitle_burn_per_eye

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::Video3DFormat;
use pharos_transcode::protocol::DeviceId;
use pharos_transcode::{Container, StereoOutput, TranscodeOptions, VideoCodec};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const TICKS_PER_SEC: u64 = 10_000_000;
/// Black limited-range luma is 16; a burned white glyph reaches 235.
const BURNED_YMAX: f64 = 150.0;

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn make_clip(dir: &Path, w: u32, h: u32) -> PathBuf {
    let clip = dir.join("packed.mkv");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s={w}x{h}:r=25:d=40"))
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg (clip)");
    assert!(status.success(), "clip generation failed");
    clip
}

fn write_srt(dir: &Path) -> PathBuf {
    let srt = dir.join("cue.srt");
    std::fs::write(&srt, "1\n00:00:29,000 --> 00:00:31,000\nMARK MARK MARK\n").expect("write srt");
    srt
}

fn burn_segment(
    input: &Path,
    srt: &Path,
    out: &Path,
    format: Video3DFormat,
    stereo_output: StereoOutput,
) {
    let opts = TranscodeOptions {
        source_video_codec: None,
        source_video_kind: Default::default(),
        source_frame_rate: None,
        container: Container::Mp4,
        video: Some(VideoCodec::H264),
        audio: None,
        audio_bitrate_bps: None,
        video_bitrate_bps: Some(1_000_000),
        // Deep enough that the setpts seek sandwich is part of the graph.
        start_position_ticks: 27 * TICKS_PER_SEC,
        duration_ticks: Some(6 * TICKS_PER_SEC),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: Some(0),
        burn_intent: true,
        burn_subtitle_is_text: true,
        burn_subtitle_ass_path: Some(srt.to_path_buf()),
        burn_fonts_dir: None,
        decode_preroll_seconds: None,
        muxed_audio_source: None,
        filler: None,
        video_3d_format: Some(format),
        stereo_output,
    };
    let args = pharos_transcode::ffmpeg_transcode_args(
        input.to_str().unwrap(),
        &opts,
        DeviceId::Cpu,
        out.to_str().unwrap(),
        pharos_transcode::DecodeOffload::Allowed,
        None,
    );
    eprintln!("argv: ffmpeg {}", args.join(" "));
    let output = Command::new("ffmpeg")
        .args(&args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg rejected the {format:?} {stereo_output:?} burn graph: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Brightest pixel (YMAX) in the region of every frame.
fn region_ymax(file: &Path, crop: &str) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-vf", &format!("{crop},signalstats,metadata=print:file=-")])
        .args(["-f", "null", "/dev/null"])
        .output()
        .expect("spawn ffmpeg (signalstats)");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split("lavfi.signalstats.YMAX=").nth(1))
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .fold(0.0_f64, f64::max)
}

fn frame_size(file: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[ignore = "requires ffmpeg; real transcode + pixel inspection"]
fn packed_3d_burn_puts_the_subtitle_in_both_eyes() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not available - skipping");
        return;
    }
    // (format, width, height, first-eye crop, second-eye crop)
    let cases = [
        (
            Video3DFormat::HalfSideBySide,
            640,
            180,
            "crop=iw/2:ih:0:0",
            "crop=iw/2:ih:iw/2:0",
        ),
        (
            Video3DFormat::FullSideBySide,
            640,
            180,
            "crop=iw/2:ih:0:0",
            "crop=iw/2:ih:iw/2:0",
        ),
        (
            Video3DFormat::HalfTopAndBottom,
            320,
            360,
            "crop=iw:ih/2:0:0",
            "crop=iw:ih/2:0:ih/2",
        ),
        (
            Video3DFormat::FullTopAndBottom,
            320,
            360,
            "crop=iw:ih/2:0:0",
            "crop=iw:ih/2:0:ih/2",
        ),
    ];
    for (format, w, h, first, second) in cases {
        let td = TempDir::new().unwrap();
        let src = make_clip(td.path(), w, h);
        let srt = write_srt(td.path());

        let out = td.path().join("per_eye.mp4");
        burn_segment(&src, &srt, &out, format, StereoOutput::Preserve);
        assert_eq!(
            frame_size(&out),
            format!("{w},{h}"),
            "{format:?}: layout must be kept"
        );
        let (a, b) = (region_ymax(&out, first), region_ymax(&out, second));
        eprintln!("{format:?} per-eye: first={a} second={b}");
        assert!(
            a > BURNED_YMAX,
            "{format:?}: first eye has no subtitle (YMAX {a})"
        );
        assert!(
            b > BURNED_YMAX,
            "{format:?}: second eye has no subtitle (YMAX {b})"
        );

        // Control: the same probe on the Mono2D (left-eye only) output sees a
        // subtitle in the single 2D frame, so the probe can tell "burned"
        // from "not burned" at this geometry.
        let mono = td.path().join("mono.mp4");
        burn_segment(&src, &srt, &mono, format, StereoOutput::Mono2D);
        let m = region_ymax(&mono, "crop=iw:ih:0:0");
        assert!(
            m > BURNED_YMAX,
            "{format:?}: mono control has no subtitle (YMAX {m})"
        );
    }
}
