//! Real-ffmpeg checks for the GPU-resident VAAPI pipeline.
//!
//! The VAAPI tests need a usable render node (`vainfo` succeeds and advertises
//! an H.264 encoder) and skip cleanly when there is none, so CI without a GPU
//! stays green. The source-inspection test needs only ffmpeg.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use pharos_transcode::options::{
    AudioCodec, Container, SourceVideoKind, TranscodeOptions, VideoCodec,
};
use pharos_transcode::protocol::DeviceId;
use pharos_transcode::resident_preflight::{ResidentBlock, SourceInspector};
use pharos_transcode::{
    ffmpeg_transcode_args_pipeline, progress_sidecar_path, DecodeOffload, HwAccel, ResidentVerdict,
    SourceCodec, VaapiPipeline,
};
use std::path::Path;
use std::process::{Command, Stdio};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// First render node `vainfo` can open that advertises an H.264 encoder, as the
/// `DeviceId` pharos would name it. `None` means "skip".
fn usable_vaapi_device() -> Option<DeviceId> {
    for idx in 0u8..8 {
        let node = format!("/dev/dri/renderD{}", 128 + u32::from(idx));
        if !Path::new(&node).exists() {
            continue;
        }
        let out = Command::new("vainfo")
            .args(["--display", "drm", "--device", &node])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if out.status.success() && text.contains("VAProfileH264") && text.contains("EncSlice") {
            return Some(DeviceId::hw(HwAccel::Vaapi, idx));
        }
    }
    None
}

fn synth(path: &Path, pix_fmt: &str, secs: u32) {
    let codec: &[&str] = if pix_fmt == "yuv420p" {
        &["-c:v", "libx264"]
    } else {
        &["-c:v", "libx265"]
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
        ])
        .arg(format!("testsrc2=duration={secs}:size=1280x720:rate=24"))
        .args(codec)
        .args(["-pix_fmt", pix_fmt, "-g", "24", "-preset", "ultrafast"])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "fixture generation failed");
}

fn opts(kind: SourceVideoKind, codec: SourceCodec) -> TranscodeOptions {
    TranscodeOptions {
        source_video_codec: Some(codec),
        source_video_kind: kind,
        source_frame_rate: pharos_core::FrameRate::from_mille(24_000),
        container: Container::Mpegts,
        video: Some(VideoCodec::H264),
        audio: None::<AudioCodec>,
        video_bitrate_bps: Some(2_000_000),
        audio_bitrate_bps: None,
        start_position_ticks: 20_000_000,
        duration_ticks: Some(30_000_000),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        decode_preroll_seconds: None,
        muxed_audio_source: None,
        video_3d_format: None,
        stereo_output: Default::default(),
        filler: None,
    }
}

/// Run the argv and return the frame count from the `-progress` sidecar.
fn run_frames(args: &[String], out: &Path) -> u64 {
    let status = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed: {args:?}");
    let sidecar = std::fs::read_to_string(progress_sidecar_path(out)).expect("sidecar");
    sidecar
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("frame=").and_then(|n| n.trim().parse().ok()))
        .expect("frame= in sidecar")
}

#[test]
fn resident_and_hybrid_encode_the_same_segment_on_real_vaapi() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let Some(dev) = usable_vaapi_device() else {
        eprintln!("skipping: no usable VAAPI render node (vainfo)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.mp4");
    synth(&input, "yuv420p", 8);
    let o = opts(SourceVideoKind::Plain8BitSdr, SourceCodec::H264);
    assert_eq!(
        ResidentVerdict::of(true, &o, dev, DecodeOffload::Allowed, false, true),
        ResidentVerdict::Resident
    );

    let frames = |pipeline: VaapiPipeline| {
        let out = dir.path().join(format!("{}.ts", pipeline.label()));
        let args = ffmpeg_transcode_args_pipeline(
            input.to_str().unwrap(),
            &o,
            dev,
            out.to_str().unwrap(),
            DecodeOffload::Allowed,
            None,
            pipeline,
        );
        (run_frames(&args, &out), out)
    };
    let (hybrid, _) = frames(VaapiPipeline::Hybrid);
    let (resident, resident_out) = frames(VaapiPipeline::Resident);
    assert_eq!(hybrid, 72, "3 s at 24 fps");
    assert_eq!(resident, hybrid, "same segment, same frame count");

    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_name,pix_fmt,width,height")
        .args(["-of", "csv=p=0"])
        .arg(&resident_out)
        .output()
        .expect("ffprobe");
    let text = String::from_utf8_lossy(&probe.stdout);
    assert!(text.trim().starts_with("h264,1280,720,yuv420p"), "{text}");
}

fn start_time(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("packet=pts_time")
        .args(["-read_intervals", "%+#1", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines()
        .next()
        .and_then(|l| l.trim().trim_end_matches(',').parse().ok())
        .unwrap_or_else(|| panic!("first pts_time not found in {text:?}"))
}

/// A mid-file segment lands on the same place on the shared timeline whichever
/// pipeline produced it: the seek, the preroll trim and `-output_ts_offset`
/// all have to agree, or consecutive segments gap or overlap at the join.
#[test]
fn a_mid_file_segment_has_the_same_frames_and_start_time_on_both_pipelines() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let Some(dev) = usable_vaapi_device() else {
        eprintln!("skipping: no usable VAAPI render node (vainfo)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.mp4");
    synth(&input, "yuv420p", 30);
    let mut o = opts(SourceVideoKind::Plain8BitSdr, SourceCodec::H264);
    // Segment 3 of a 6 s grid; the 15 s preroll puts the decoder seek at 3 s.
    o.start_position_ticks = 180_000_000;
    o.duration_ticks = Some(60_000_000);

    let run = |pipeline: VaapiPipeline| {
        let out = dir.path().join(format!("mid_{}.ts", pipeline.label()));
        let args = ffmpeg_transcode_args_pipeline(
            input.to_str().unwrap(),
            &o,
            dev,
            out.to_str().unwrap(),
            DecodeOffload::Allowed,
            None,
            pipeline,
        );
        (run_frames(&args, &out), start_time(&out))
    };
    let (hybrid_frames, hybrid_start) = run(VaapiPipeline::Hybrid);
    let (resident_frames, resident_start) = run(VaapiPipeline::Resident);
    assert_eq!(hybrid_frames, 144);
    assert_eq!(resident_frames, hybrid_frames);
    assert!(
        (hybrid_start - 18.0).abs() < 0.1,
        "hybrid should start at the segment's timeline position: {hybrid_start}"
    );
    assert!(
        (resident_start - hybrid_start).abs() < 1e-3,
        "start_time differs: hybrid {hybrid_start} resident {resident_start}"
    );
}

#[test]
fn a_ten_bit_source_is_not_resident_and_still_encodes_on_the_hybrid_path() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let Some(dev) = usable_vaapi_device() else {
        eprintln!("skipping: no usable VAAPI render node (vainfo)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in10.mp4");
    synth(&input, "yuv420p10le", 8);
    let kind = SourceVideoKind::classify(Some("yuv420p10le"), Some("bt709"), Some(1280), Some(720));
    assert_eq!(kind, SourceVideoKind::Other);
    let o = opts(kind, SourceCodec::Hevc);
    let verdict = ResidentVerdict::of(true, &o, dev, DecodeOffload::Allowed, false, true);
    assert_eq!(verdict, ResidentVerdict::SourceNotPlain);

    let out = dir.path().join("out.ts");
    let args = ffmpeg_transcode_args_pipeline(
        input.to_str().unwrap(),
        &o,
        dev,
        out.to_str().unwrap(),
        DecodeOffload::Allowed,
        None,
        verdict.pipeline(),
    );
    assert_eq!(run_frames(&args, &out), 72);
}

#[tokio::test]
async fn the_inspector_blocks_a_rotated_and_an_interlaced_source_and_passes_a_plain_one() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.mp4");
    synth(&plain, "yuv420p", 2);

    let rotated = dir.path().join("rot.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-display_rotation",
            "90",
            "-i",
        ])
        .arg(&plain)
        .args(["-c", "copy"])
        .arg(&rotated)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());

    let interlaced = dir.path().join("tff.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc2=duration=2:size=640x360:rate=25")
        .args(["-vf", "interlace=scan=tff", "-flags", "+ildct+ilme"])
        .args([
            "-c:v",
            "libx264",
            "-x264-params",
            "tff=1",
            "-preset",
            "ultrafast",
        ])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&interlaced)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());

    let inspector = SourceInspector::new("ffmpeg");
    assert_eq!(inspector.inspect(&plain).await, None);
    assert_eq!(
        inspector.inspect(&rotated).await,
        Some(ResidentBlock::Rotated)
    );
    assert_eq!(
        inspector.inspect(&interlaced).await,
        Some(ResidentBlock::Interlaced)
    );
    assert_eq!(
        inspector.inspect(&dir.path().join("missing.mp4")).await,
        Some(ResidentBlock::Unreadable),
        "a file ffmpeg cannot open is not a clean bill of health"
    );
}
