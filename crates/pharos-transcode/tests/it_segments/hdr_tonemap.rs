//! An HDR10 source transcodes to an SDR (BT.709) segment, and an untonemapped
//! one keeps its PQ tags (ADR-0070).
//!
//! Requires `ffmpeg`/`ffprobe` with libzimg on PATH: the devShell and CI have
//! them, so a missing filter fails loudly instead of skipping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, AudioDelivery, SegmentContainer, SegmentOpts,
    SegmentVideo, Tonemap,
};

fn make_hdr10_source(dir: &Path) -> PathBuf {
    let src = dir.join("hdr10.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=4:size=320x180:rate=24",
            "-vf",
            "format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error",
            "-pix_fmt",
            "yuv420p10le",
            "-y",
        ])
        .arg(&src)
        .output()
        .expect("spawn ffmpeg (is it on PATH? run inside the devShell)");
    assert!(
        out.status.success(),
        "source synth failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        probe(&src, "color_transfer"),
        "smpte2084",
        "fixture is not HDR10, guard is vacuous"
    );
    src
}

fn probe(path: &Path, field: &str) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg(format!("stream={field}"))
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn encode(src: &Path, dir: &Path, tonemap: Tonemap) -> PathBuf {
    let opts = SegmentOpts {
        source_video_codec: None,
        source_video_kind: Default::default(),
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(1_000_000),
        window: pharos_core::SegmentWindow::for_segment(
            0,
            pharos_core::FrameRate::from_mille(24_000),
            Some(4.0),
        ),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        video_3d_format: None,
        stereo_output: Default::default(),
        tonemap,
    };
    let out = dir.join(format!("seg-{}.m4s", tonemap.label()));
    let args = ffmpeg_transcode_args(
        src.to_str().unwrap(),
        &opts
            .resolve_with(|_| Err::<pharos_transcode::MuxedAudio, ()>(()))
            .expect("audio-free segment resolves without a slice")
            .to_transcode_options(),
        DeviceId::Cpu,
        out.to_str().unwrap(),
        pharos_transcode::DecodeOffload::Allowed,
        None,
    );
    let res = Command::new("ffmpeg")
        .args(&args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        res.status.success(),
        "encode failed: {}\nargs: {}",
        String::from_utf8_lossy(&res.stderr),
        args.join(" ")
    );
    out
}

#[test]
fn an_hdr10_source_comes_out_bt709_sdr_when_tonemapped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = make_hdr10_source(dir.path());
    let out = encode(&src, dir.path(), Tonemap::HdrToSdr);
    assert_eq!(probe(&out, "pix_fmt"), "yuv420p");
    assert_eq!(probe(&out, "color_primaries"), "bt709");
    assert_eq!(probe(&out, "color_space"), "bt709");
    assert_eq!(probe(&out, "color_transfer"), "bt709");
}

#[test]
fn without_tonemap_the_output_is_not_bt709() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = make_hdr10_source(dir.path());
    let out = encode(&src, dir.path(), Tonemap::Off);
    // The contrast case: shows the assertions above can go red.
    assert_ne!(probe(&out, "color_primaries"), "bt709");
}
