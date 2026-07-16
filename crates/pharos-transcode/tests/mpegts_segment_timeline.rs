//! B45 — behavioral guard on the mpegts HLS segment timeline.
//!
//! B41 added `-output_ts_offset` to `.ts` segment args and unit-tested that
//! the STRING appears in the argv — but never that ffmpeg honors it. It
//! doesn't, under `-c:v copy` (the flag is silently inert on the copy path,
//! ffmpeg 8.1), and even re-encoded segments carried a +1.4 s skew from the
//! mpegts muxer's default initial cue delay. Both shipped. This test runs
//! the REAL ffmpeg binary over the exact argv `ffmpeg_transcode_args`
//! produces and asserts the segment's actual timestamps, so an arg the
//! muxer ignores can never masquerade as a fix again.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and
//! CI; the test fails loudly (not skips) if they're missing, so a broken
//! environment can't silently drop the guard.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use pharos_core::time::TICKS_PER_SECOND;

fn ffmpeg() -> &'static str {
    "ffmpeg"
}

fn ffprobe() -> &'static str {
    "ffprobe"
}

/// 20 s synthetic h264 + 5.1 AAC source (keyframes every 2 s so copy-vs-
/// re-encode cut behavior would differ observably).
fn make_source(dir: &std::path::Path) -> std::path::PathBuf {
    let src = dir.join("src.mkv");
    let out = Command::new(ffmpeg())
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=20:size=320x180:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "50",
            "-c:a",
            "aac",
            "-ac",
            "6",
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

fn probe_json(path: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(ffprobe())
        .args(["-v", "error", "-of", "json"])
        .args(args)
        .arg(path)
        .output()
        .expect("spawn ffprobe (is it on PATH? run inside the devShell)");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ffprobe json")
}

/// Transcode one 6 s segment starting at 6.0 s through the production argv
/// and return the emitted file.
fn transcode_segment(dir: &std::path::Path, src: &std::path::Path) -> std::path::PathBuf {
    let seg = dir.join("seg1.ts");
    let opts = pharos_transcode::TranscodeOptions {
        container: pharos_transcode::Container::Mpegts,
        video: Some(pharos_transcode::VideoCodec::H264),
        audio: Some(pharos_transcode::AudioCodec::Aac),
        video_bitrate_bps: Some(1_000_000),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: 6 * TICKS_PER_SECOND,
        duration_ticks: Some(6 * TICKS_PER_SECOND),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
    };
    let args = pharos_transcode::ffmpeg_transcode_args(
        src.to_str().expect("utf8 tmpdir"),
        &opts,
        pharos_transcode::protocol::DeviceId::Cpu,
        seg.to_str().expect("utf8 tmpdir"),
    );
    let out = Command::new(ffmpeg())
        .args(["-v", "error"])
        .args(&args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "segment transcode failed: {}\nargs: {args:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    seg
}

#[test]
fn mid_timeline_segment_carries_true_pts_and_stereo_audio() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let src = make_source(dir.path());
    let seg = transcode_segment(dir.path(), &src);

    // 1. Timeline anchor: the segment must START at its true position on
    //    the shared clock (6.0 s ± encoder/mux jitter), NOT at 0 (offset
    //    ignored) and NOT at 7.4 s (default 1.4 s muxdelay skew).
    let fmt = probe_json(&seg, &["-show_entries", "format=start_time,duration"]);
    let start: f64 = fmt["format"]["start_time"]
        .as_str()
        .expect("start_time")
        .parse()
        .expect("numeric start_time");
    assert!(
        (start - 6.0).abs() < 0.5,
        "segment start_time {start} not anchored at ~6.0 s — \
         -output_ts_offset/-muxdelay not honored by the muxer"
    );

    // 2. Grid tiling: actual duration must match the requested 6 s cut
    //    (a stream-copied segment can only cut on keyframes and drifts).
    let dur: f64 = fmt["format"]["duration"]
        .as_str()
        .expect("duration")
        .parse()
        .expect("numeric duration");
    assert!(
        (dur - 6.0).abs() < 0.5,
        "segment duration {dur} off the 6 s EXTINF grid"
    );

    // 3. Audio: 5.1 source must land as stereo AAC — multichannel AAC is
    //    undecodable in Firefox's MSE and kills playback on segment 0.
    let streams = probe_json(&seg, &["-show_entries", "stream=codec_type,channels"]);
    let channels = streams["streams"]
        .as_array()
        .expect("streams")
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .expect("audio stream")["channels"]
        .as_i64()
        .expect("channels");
    assert_eq!(channels, 2, "audio must downmix to stereo for MSE compat");
}
