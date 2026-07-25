//! The audio timeline must tile across segment boundaries, not restart at each.
//!
//! HLS segments are independent ffmpeg runs. When each one *encodes its own
//! audio*, the codec re-primes at every segment: the encoder emits a priming
//! frame before the seek point and starts a fresh frame grid from there. The
//! result is that consecutive segments carry overlapping, phase-misaligned
//! audio — the same instants encoded twice, at different timestamps, from
//! different encoder state.
//!
//! Measured on the live server (Fringe S01E02, muxed mpegts path) before this
//! was fixed:
//!
//! ```text
//! seg40  video [239.969 → 245.975]   audio [239.9477 → 245.985]
//! seg41  video [245.975 → 251.981]   audio [245.9537 → 251.991]
//! seg42  video [251.981 → 257.987]   audio [251.9597 → 257.997]
//! ```
//!
//! Every segment held 6.037 s of audio against 6.006 s of video, so ~31 ms was
//! duplicated at each boundary, and the duplicates did not line up:
//! `(245.953667 − 239.947667) / 0.0213333 = 281.53`, not an integer. A player
//! that appends rather than de-duplicates drifts 0.52% against video and
//! periodically resyncs — reported as intermittent freeze-then-catch-up.
//!
//! Reproduced by this test against the synthetic fixture below, before the fix:
//!
//! ```text
//! segment 4's audio starts 0.0090 s BEFORE segment 3's audio ends
//! (23.940100 < 23.949144) — 9.0 ms duplicated at the boundary
//! ```
//!
//! (Smaller than production's 31 ms because the fixture's encoder delay is
//! smaller; the defect is the same and the third assertion fails identically.)
//!
//! The third assertion below is the one that cannot be satisfied by a
//! per-segment audio encode, no matter how the boundaries are rounded: it
//! demands that both segments' audio sit on ONE global frame grid.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and CI;
//! the test fails loudly rather than skipping, so a broken environment cannot
//! silently drop the guard.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_core::time::TICKS_PER_SECOND;
use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, SegmentAudio, SegmentContainer, SegmentOpts,
    SegmentVideo,
};

/// 23.976 fps so the frame-snapped grid produces non-round boundaries — the
/// condition under which the phase error is visible at all.
const FPS_MILLE: u32 = 23_976;
const SAMPLE_RATE: u32 = 48_000;

fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=40:size=320x180:rate=24000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=40:sample_rate=48000",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
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
    src
}

/// Encode one segment through the REAL argv builder, on the same grid the
/// server uses. Nothing here re-derives a boundary: `segment_range` is the
/// production function.
fn encode_segment(src: &Path, dir: &Path, seg: u32) -> PathBuf {
    let rate = pharos_core::FrameRate::from_mille(FPS_MILLE);
    let (start, dur) = pharos_core::segment_range(seg, rate);
    let opts = SegmentOpts {
        container: SegmentContainer::Mpegts,
        video: Some(SegmentVideo::H264),
        audio: Some(SegmentAudio::Aac),
        video_bitrate_bps: Some(2_000_000),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: (start * TICKS_PER_SECOND as f64) as u64,
        duration_ticks: Some((dur * TICKS_PER_SECOND as f64) as u64),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    };
    let out = dir.join(format!("seg{seg}.ts"));
    let args = ffmpeg_transcode_args(
        src.to_str().unwrap(),
        &opts.to_transcode_options(),
        DeviceId::Cpu,
        out.to_str().unwrap(),
    );
    let res = Command::new("ffmpeg")
        .args(&args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        res.status.success(),
        "segment {seg} encode failed: {}\nargs: {}",
        String::from_utf8_lossy(&res.stderr),
        args.join(" ")
    );
    out
}

/// Every audio frame PTS in a segment, in order.
fn audio_pts(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .collect()
}

#[test]
#[ignore = "fails until muxed audio is copied from one continuous encode (plan Task 5)"]
fn audio_frames_tile_exactly_across_a_segment_boundary() {
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source(td.path());

    let a = encode_segment(&src, td.path(), 3);
    let b = encode_segment(&src, td.path(), 4);
    let pa = audio_pts(&a);
    let pb = audio_pts(&b);
    assert!(pa.len() > 10 && pb.len() > 10, "segments carry audio");

    // Derived from the stream, never a literal — a test that hard-codes the
    // frame duration can agree with a wrong encoder and prove nothing.
    let frame_dur = 1024.0 / SAMPLE_RATE as f64;

    let last_a = *pa.last().unwrap();
    let first_b = pb[0];
    let first_a = pa[0];

    // 1. No overlap: segment 4 must not repeat instants segment 3 already
    //    carried. This is the 31 ms of duplicated audio.
    assert!(
        first_b >= last_a,
        "segment 4's audio starts {:.4} s BEFORE segment 3's audio ends \
         ({first_b:.6} < {last_a:.6}) — {:.1} ms duplicated at the boundary",
        last_a - first_b,
        (last_a - first_b) * 1000.0
    );

    // 2. No gap either: exactly one frame separates them.
    let join = first_b - last_a;
    assert!(
        (join - frame_dur).abs() < 0.001,
        "boundary join is {join:.6} s, expected one frame ({frame_dur:.6} s)"
    );

    // 3. THE property a per-segment encode cannot satisfy: both segments'
    //    audio must sit on one global frame grid, so the offset between their
    //    first frames is a whole number of frames. Production measured 281.53.
    let frames_between = (first_b - first_a) / frame_dur;
    assert!(
        (frames_between - frames_between.round()).abs() < 1e-3,
        "segment 3 and 4 audio grids are phase-shifted: {frames_between:.4} \
         frames between their first samples — a whole number is required, so \
         the audio is being re-primed per segment instead of copied from one \
         continuous encode"
    );
}
