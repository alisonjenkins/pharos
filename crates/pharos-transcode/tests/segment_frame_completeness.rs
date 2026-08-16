//! A segment must carry the frames its advertised duration implies.
//!
//! Every HLS segment is an independent `ffmpeg -ss START -t DUR` run, so its
//! decoder starts cold at `START`. ffmpeg seeks to whatever the container
//! index advertises as the nearest random-access point — and when that index
//! is wrong, the decoder cannot rebuild its reference picture set there:
//!
//! ```text
//! [hevc] Could not find ref with POC -41
//! [hevc] Error constructing the frame RPS.
//! [hevc] Skipping invalid undecodable NALU: 1
//! --- exit=0 ---
//! ```
//!
//! It discards frames until it reaches a genuine recovery point, and **exits
//! 0**. That last line is why this reached production and stayed invisible:
//! the exit-status gate, the minimum-byte floor, the cache and the ETag all
//! read the segment as a success, and audio was still muxed in, so the file
//! was not even empty. Measured on the source that surfaced it (Fringe S01E02,
//! at the four timestamps reported as freezes): 9 of 43 sampled segments short
//! of frames, two containing no video at all.
//!
//! The guard here is deliberately the INVARIANT rather than that one decoder
//! state: a segment that does not carry its frames is broken regardless of
//! which container quirk or codec caused it. Reproducing the exact reference
//! failure would need a container with a lying sync-sample table and would
//! test one mechanism instead of the property.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and
//! CI; the test fails loudly rather than skipping, so a broken environment
//! cannot silently drop the guard.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, AudioDelivery, SegmentContainer, SegmentOpts,
    SegmentVideo,
};

/// 23.976 fps, so the frame-snapped grid produces non-round boundaries — the
/// case where an off-by-a-frame error is actually visible.
const FPS_MILLE: u32 = 23_976;

fn frame_rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(FPS_MILLE).expect("23.976 is a valid frame rate")
}

fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=60:size=320x180:rate=24000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=60:sample_rate=48000",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            // A long GOP so a mid-file segment genuinely has to seek back and
            // decode into the middle of one, rather than landing on a keyframe
            // every couple of seconds and never exercising the path.
            "-g",
            "240",
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

/// Encode one segment through the REAL argv builder, on the production grid.
/// Nothing here re-derives a boundary or an ffmpeg flag.
fn encode_segment(src: &Path, dir: &Path, seg: u32) -> (PathBuf, f64, f64) {
    let (start, dur) = pharos_core::segment_range(seg, Some(frame_rate()));
    let opts = SegmentOpts {
        source_video_codec: None,
        container: SegmentContainer::Mpegts,
        video: Some(SegmentVideo::H264),
        // Video-only: this test counts VIDEO frames, and it never had a
        // continuous-audio encode to copy from. It declared `Muxed` with no
        // source, which silently produced a video-only segment anyway — the
        // exact contradiction `ResolvedAudio` now makes unrepresentable. Saying
        // `Separate` states what the test actually encodes.
        audio: AudioDelivery::Separate,

        video_bitrate_bps: Some(2_000_000),
        window: pharos_core::SegmentWindow::for_segment(seg, Some(frame_rate()), Some(60.0)),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    };
    let out = dir.join(format!("seg{seg}.ts"));
    let args = ffmpeg_transcode_args(
        src.to_str().unwrap(),
        &opts
            .resolve_with(|_| Err::<pharos_transcode::MuxedAudio, ()>(()))
            .expect("audio-free segment resolves without a slice")
            .to_transcode_options(),
        DeviceId::Cpu,
        out.to_str().unwrap(),
        pharos_transcode::DecodeOffload::Allowed,
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
    (out, start, dur)
}

/// Every video frame PTS in a segment, in order.
fn video_pts(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    let mut v: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

#[test]
fn a_mid_file_segment_carries_every_frame_its_duration_implies() {
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source(td.path());
    let rate = frame_rate();

    // Segments deep enough that the preroll is not clamped by the start of
    // the file, and spanning a GOP boundary in both directions.
    for seg in [4_u32, 5, 6, 7] {
        let (path, start, dur) = encode_segment(&src, td.path(), seg);
        let pts = video_pts(&path);

        // Independently derived from the grid and the frame rate — never from
        // the encoder's own count, which is the thing under test.
        let expected = (dur * rate.fps()).round() as usize;
        assert!(
            pts.len() + 1 >= expected,
            "segment {seg} advertises {dur:.6} s at {:.4} fps ({expected} \
             frames) but carries {} — the decoder silently dropped frames and \
             ffmpeg still exited 0",
            rate.fps(),
            pts.len()
        );
        assert!(
            pts.len() <= expected + 1,
            "segment {seg} carries {} frames, more than the {expected} its \
             {dur:.6} s duration implies — it is overlapping its neighbour",
            pts.len()
        );

        // The preroll must not move the segment: the frames that survive the
        // output-side trim have to start at the segment's own position on the
        // shared timeline, not at the earlier point the decoder was seeded
        // from. A preroll that forgot the trim would put the first frame
        // `DECODE_PREROLL_SECONDS` early and pass the count check above.
        let frame = 1.0 / rate.fps();
        assert!(
            (pts[0] - start).abs() < frame,
            "segment {seg} starts at {:.6} s, not its grid position \
             {start:.6} s (one frame is {frame:.6} s)",
            pts[0]
        );
    }
}

#[test]
fn ffmpeg_reports_the_frames_it_wrote_next_to_the_segment() {
    // The runtime guard in pharos-cache decides whether a segment is complete
    // from ffmpeg's `-progress` sidecar, because the exit status cannot tell
    // it. That only works if ffmpeg genuinely writes the report and its count
    // agrees with the file it produced — assert both against a real encode
    // rather than trusting the flag.
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source(td.path());
    let (path, _, _) = encode_segment(&src, td.path(), 4);
    let probed = video_pts(&path).len();

    let sidecar = pharos_transcode::progress_sidecar_path(&path);
    let report = std::fs::read_to_string(&sidecar)
        .unwrap_or_else(|e| panic!("no progress report at {}: {e}", sidecar.display()));
    let reported: usize = report
        .lines()
        .filter_map(|l| l.strip_prefix("frame="))
        .next_back()
        .expect("progress report carries a frame count")
        .trim()
        .parse()
        .expect("frame count is a number");

    assert_eq!(
        reported, probed,
        "ffmpeg reported {reported} frames but the segment holds {probed}"
    );
    assert!(
        report.contains("out_time_us="),
        "progress report must carry out_time_us, the truncation signal: \
         {report}"
    );
}

#[test]
fn consecutive_segments_join_without_a_gap_or_a_repeat() {
    // The count check alone cannot see a segment that carries the right
    // number of frames from the wrong place. Two adjacent segments must meet
    // exactly one frame apart.
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source(td.path());
    let rate = frame_rate();
    let frame = 1.0 / rate.fps();

    let (a, _, _) = encode_segment(&src, td.path(), 4);
    let (b, _, _) = encode_segment(&src, td.path(), 5);
    let pa = video_pts(&a);
    let pb = video_pts(&b);

    let join = pb[0] - pa.last().unwrap();
    assert!(
        (join - frame).abs() < frame * 0.5,
        "segments 4 and 5 join {join:.6} s apart; exactly one frame \
         ({frame:.6} s) is required — anything else drops or repeats the \
         boundary frame"
    );
}
