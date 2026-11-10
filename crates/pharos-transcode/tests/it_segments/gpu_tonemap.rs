//! The GPU tonemap route on a real GPU (ADR-0072): Vulkan decode, libplacebo,
//! readback, VAAPI encode, against the CPU zscale chain on the same source.
//!
//! Needs ffmpeg with libplacebo, a VAAPI render node and a Vulkan ICD for it.
//! Each is detected, and the test skips cleanly when any is missing, so CI
//! without a GPU stays green. The skip decision is the server's own trial
//! (`probe_gpu_tonemap`), so a box where the route would be disabled in
//! production skips here too.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};

use pharos_transcode::device::GpuTonemapCapability;
use pharos_transcode::options::{Container, TranscodeOptions, VideoCodec};
use pharos_transcode::{
    ffmpeg_transcode_args_pipeline, DecodeOffload, SourceCodec, Tonemap, VaapiPipeline,
};

fn ffmpeg_has_libplacebo() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(" libplacebo "))
        .unwrap_or(false)
}

fn hdr_job() -> TranscodeOptions {
    TranscodeOptions {
        source_video_codec: Some(SourceCodec::Hevc),
        source_video_kind: Default::default(),
        source_frame_rate: pharos_core::FrameRate::from_mille(24_000),
        container: Container::Mpegts,
        video: Some(VideoCodec::H264),
        audio: None,
        video_bitrate_bps: Some(4_000_000),
        audio_bitrate_bps: None,
        start_position_ticks: 0,
        duration_ticks: None,
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
        tonemap: Tonemap::HdrToSdr,
        output_size: None,
        filler: None,
    }
}

fn synth_hdr10(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=4:size=1280x720:rate=24",
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
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "HDR10 fixture generation failed");
}

/// Run the argv for `pipeline` and return its output path.
fn transcode(
    input: &Path,
    out: &Path,
    device: pharos_transcode::protocol::DeviceId,
    p: VaapiPipeline,
) {
    let args = ffmpeg_transcode_args_pipeline(
        &input.to_string_lossy(),
        &hdr_job(),
        device,
        &out.to_string_lossy(),
        DecodeOffload::Denied,
        None,
        p,
    );
    let o = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn ffmpeg");
    assert!(
        o.status.success(),
        "{p:?} failed: {}\nargv: {args:?}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// (frames, mean luma, max luma) of `path`, from 320x180 gray frames.
fn luma(path: &Path) -> (usize, f64, u8) {
    let o = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-vf",
            "scale=320:180:flags=area,format=gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("spawn ffmpeg");
    assert!(o.status.success());
    let frame = 320 * 180;
    let sum: u64 = o.stdout.iter().map(|&b| u64::from(b)).sum();
    (
        o.stdout.len() / frame,
        sum as f64 / o.stdout.len() as f64,
        o.stdout.iter().copied().max().unwrap_or(0),
    )
}

#[test]
fn gpu_route_matches_the_cpu_chain_on_a_real_gpu() {
    if !ffmpeg_has_libplacebo() {
        eprintln!("skipping: ffmpeg has no libplacebo");
        return;
    }
    let Some(node) = (128u8..136)
        .map(|n| (n, format!("/dev/dri/renderD{n}")))
        .find(|(_, p)| Path::new(p).exists())
    else {
        eprintln!("skipping: no DRM render node");
        return;
    };
    let device =
        pharos_transcode::protocol::DeviceId::hw(pharos_transcode::HwAccel::Vaapi, node.0 - 128);
    let verdict = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(pharos_transcode::probe::probe_gpu_tonemap(
            device,
            pharos_transcode::probe::GPU_TONEMAP_TRIAL_TIMEOUT,
        ));
    if verdict != GpuTonemapCapability::Capable {
        eprintln!("skipping: {device} cannot run the GPU tonemap route (see the trial's warning)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("hdr10.mkv");
    synth_hdr10(&src);
    let gpu_out = dir.path().join("gpu.ts");
    let cpu_out = dir.path().join("cpu.ts");
    transcode(&src, &gpu_out, device, VaapiPipeline::GpuTonemap);
    transcode(&src, &cpu_out, device, VaapiPipeline::Hybrid);

    let (gf, gmean, gmax) = luma(&gpu_out);
    let (cf, cmean, cmax) = luma(&cpu_out);
    eprintln!(
        "gpu: {gf} frames mean {gmean:.1} max {gmax}; cpu: {cf} frames mean {cmean:.1} max {cmax}"
    );
    assert_eq!(gf, cf, "the routes must produce the same frame count");
    assert_eq!(gf, 4 * 24, "every source frame comes out");
    // The routes differ by design: libplacebo (BT.2390, 203 nit reference
    // white) against the zscale chain (mobius, 203 nit, ADR-0091). This bound
    // is loose; it only catches an output that is not tonemapped at all or is
    // black.
    assert!(
        (40.0..220.0).contains(&gmean) && gmax > 100,
        "implausible GPU luma: mean {gmean:.1} max {gmax}"
    );
    assert!(
        (gmean - cmean).abs() < 60.0,
        "mean luma diverges: gpu {gmean:.1} vs cpu {cmean:.1}"
    );
}
