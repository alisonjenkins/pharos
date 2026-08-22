//! B201/V154 — a filler segment, produced by a REAL ffmpeg through the
//! production argv builder, must actually be a servable segment: it must
//! carry exactly the frames its window implies, span the window's true
//! duration, land at the window's true position on the shared timeline (like
//! a real sibling segment of the same rendition), and carry audio only when
//! the rendition it stands in for muxes audio.
//!
//! `crates/pharos-cache/src/hls_cache.rs`'s unit tests drive a FAKE scheduler
//! worker that never looks at the argv at all, so a broken filler argv (one
//! that, say, still tried to decode the real — damaged — input) would 500
//! exactly as B201 did while every one of those tests stayed green. This is
//! the test that actually runs the argv `build_args_for_device` emits for a
//! filler window through the real `ffmpeg` binary and inspects the bytes.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and
//! CI (see `flake.nix`); fails loudly rather than skipping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, AudioDelivery, FillerSpec, SegmentContainer,
    SegmentOpts, SegmentVideo,
};

/// 23.976 fps — the cadence the real B201 incident (and most of this
/// library) uses, and the one the task's own expected frame count (144 over
/// 6.006 s) is stated against.
fn frame_rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(23_976).expect("23.976 is a valid frame rate")
}

/// A short real source, purely so a REAL sibling segment can be encoded
/// alongside the filler for the timeline-anchor comparison. The filler
/// itself never opens this file.
fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=60:size=128x72:rate=24000/1001",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "240",
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

/// A window that is nonzero-start, non-final, mid-file: segment 4 of a
/// 60 s/23.976 fps grid — the same scale `segment_frame_completeness.rs`
/// already proves is representative, just far cheaper to synthesize than a
/// literal 5580 s source.
fn window() -> pharos_core::SegmentWindow {
    pharos_core::SegmentWindow::for_segment(4, Some(frame_rate()), Some(60.0))
}

/// `AudioDelivery::Separate` regardless of whether the FILLER carries audio:
/// a filler never copies real audio (see `FillerSpec::audio_codec`'s doc
/// comment), it substitutes silence, which is expressed entirely on
/// `FillerSpec` at the call site rather than through a real `MuxedAudio`
/// source here.
fn segment_opts() -> SegmentOpts {
    SegmentOpts {
        source_video_codec: None,
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(1_000_000),
        window: window(),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    }
}

/// Encode one FILLER segment through the real production argv builder and
/// the real `ffmpeg` binary.
fn encode_filler(dir: &Path, out_name: &str, spec: FillerSpec) -> PathBuf {
    let opts = segment_opts()
        .resolve_with(|_| Err::<pharos_transcode::MuxedAudio, ()>(()))
        .expect("filler never asks for a muxed-audio slice")
        .to_transcode_options();
    let out = dir.join(out_name);
    let args = ffmpeg_transcode_args(
        // The filler never opens this — any path is fine, and using an
        // obviously-fake one pins that fact: if the filler branch ever fell
        // through to the real-decode path, ffmpeg would fail outright on a
        // nonexistent input instead of silently succeeding against it.
        "/nonexistent/damaged-source.mkv",
        &opts,
        DeviceId::Cpu,
        out.to_str().unwrap(),
        pharos_transcode::DecodeOffload::Allowed,
        Some(spec),
    );
    let res = Command::new("ffmpeg")
        .args(&args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        res.status.success(),
        "filler encode failed: {}\nargs: {}",
        String::from_utf8_lossy(&res.stderr),
        args.join(" ")
    );
    out
}

/// Encode the REAL sibling segment at the same window, from a real source —
/// used only to compare timeline placement against the filler.
fn encode_real_sibling(src: &Path, dir: &Path) -> PathBuf {
    let opts = segment_opts()
        .resolve_with(|_| Err::<pharos_transcode::MuxedAudio, ()>(()))
        .expect("video-only segment resolves without a slice")
        .to_transcode_options();
    let out = dir.join("real_sibling.m4s");
    let args = ffmpeg_transcode_args(
        src.to_str().unwrap(),
        &opts,
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
        "real sibling encode failed: {}\nargs: {}",
        String::from_utf8_lossy(&res.stderr),
        args.join(" ")
    );
    out
}

fn probe_json(path: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new("ffprobe")
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

/// Every frame PTS (in absolute, `-output_ts_offset`-anchored seconds) on
/// the named stream (`"v"` or `"a"`), sorted.
///
/// NOT `ffprobe format=duration`/`format=start_time`: on a headerless
/// `empty_moov` fragmented mp4 (this container — see the `-movflags` block
/// in `build_args_for_device`) those fields are unreliable — measured while
/// building this test, `format.duration` on a REAL (non-filler) segment
/// through this exact argv reported ~30 s for a 6.006 s window (it reads
/// closer to the absolute END position than the span). The per-PACKET PTS
/// values are not: they come from the actual encoded timestamps, which is
/// what a player/muxer downstream (and `segment_frame_completeness.rs`'s
/// `video_pts`) already reads instead.
fn stream_pts(path: &Path, select: &str) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            select,
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
        .filter_map(|l| l.trim().parse::<f64>().ok())
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

fn format_start_time(path: &Path) -> f64 {
    let fmt = probe_json(path, &["-show_entries", "format=start_time"]);
    fmt["format"]["start_time"]
        .as_str()
        .expect("start_time")
        .parse()
        .expect("numeric start_time")
}

fn has_audio_stream(path: &Path) -> bool {
    let streams = probe_json(path, &["-show_entries", "stream=codec_type"]);
    streams["streams"]
        .as_array()
        .expect("streams")
        .iter()
        .any(|s| s["codec_type"] == "audio")
}

/// The core V154 pin: a filler for a mid-file, nonzero-start window carries
/// exactly the frames the window implies, spans the window's true duration,
/// and lands at the window's true position — the three properties that make
/// it a SERVABLE segment rather than a black rectangle of the wrong shape or
/// in the wrong place on the playlist.
#[test]
fn a_filler_segment_carries_the_windows_true_frame_count_duration_and_position() {
    let td = tempfile::TempDir::new().unwrap();
    let rate = frame_rate();
    let w = window();

    let spec = FillerSpec {
        width: 128,
        height: 72,
        audio_codec: None,
    };
    let filler = encode_filler(td.path(), "filler.m4s", spec);

    let pts = stream_pts(&filler, "v");
    // Independently derived from the grid/frame-rate, exactly like the real
    // segment guard in `segment_frame_completeness.rs` — never hardcoded,
    // though the task's own worked example (144 frames over a 6.006 s window
    // at 24000/1001) is exactly this arithmetic for a mid-file segment.
    let expected = (w.duration_seconds() * rate.fps()).round() as usize;
    assert_eq!(
        expected, 144,
        "sanity: this window is the task's own worked example"
    );
    // ±1 frame — the same tolerance `segment_frame_completeness.rs` uses for
    // this exact measurement: 6.006 s at 24000/1001 is EXACTLY 144 frames, so
    // the `-t` cut lands precisely on a frame boundary and whether ffmpeg
    // includes or excludes that boundary frame is a rounding artifact, not a
    // dropped/duplicated frame in the sense either harness exists to catch.
    assert!(
        pts.len() + 1 >= expected && pts.len() <= expected + 1,
        "filler window is {:.6} s at {:.4} fps ({expected} frames, ±1) but \
         the encoded filler carries {}",
        w.duration_seconds(),
        rate.fps(),
        pts.len()
    );
    assert!(
        (w.duration_seconds() - 6.006).abs() < 0.001,
        "{}",
        w.duration_seconds()
    );

    // Span, not `ffprobe format=duration` — see `stream_pts`'s doc comment
    // for why that field is unreliable on this container.
    let frame = 1.0 / rate.fps();
    let span = pts.last().expect("nonempty") - pts.first().expect("nonempty") + frame;
    assert!(
        (span - w.duration_seconds()).abs() < frame * 1.5,
        "filler pts span {span:.6} s does not match the window's \
         {:.6} s",
        w.duration_seconds()
    );

    // Timeline anchor: a filler must land exactly where a REAL sibling
    // segment of this rendition would — both are driven by the same
    // `-output_ts_offset`/`start_position_ticks`, untouched by the filler
    // branch (see `build_args_for_device`'s doc comment on `start`).
    let src = make_source(td.path());
    let real = encode_real_sibling(&src, td.path());
    let real_pts = stream_pts(&real, "v");
    // Coarse tolerance against the NOMINAL window start: mux-level jitter
    // (encoder priming, the exact-boundary rounding noted above) can shift
    // the first frame by a couple of frames without the segment being
    // mis-anchored — the tight, meaningful comparison is against the REAL
    // sibling right below, which is driven by the identical
    // `-output_ts_offset` arithmetic and must therefore agree closely.
    assert!(
        (pts[0] - w.start_seconds()).abs() < 0.5,
        "filler's first video pts {} is not anchored near the window's true \
         start {:.6}",
        pts[0],
        w.start_seconds()
    );
    assert!(
        (pts[0] - real_pts[0]).abs() < frame * 2.0,
        "filler's first video pts {} disagrees with its real sibling's {} \
         — they must land on the same shared timeline slot",
        pts[0],
        real_pts[0]
    );
    // `format=start_time` corroborates the same anchor from the container
    // side, on both encodes, so the pin is not resting on frame PTS alone.
    let (filler_start, real_start) = (format_start_time(&filler), format_start_time(&real));
    assert!(
        (filler_start - real_start).abs() < 0.5,
        "filler container start_time {filler_start} disagrees with its \
         real sibling's {real_start}"
    );
}

/// A muxed-audio rendition's filler still carries an audio track — silence,
/// matched to the window's duration — rather than reproducing the
/// video-only omission a real segment of that rendition would never have.
#[test]
fn a_muxed_audio_renditions_filler_carries_silence_matched_to_the_window() {
    let td = tempfile::TempDir::new().unwrap();
    let w = window();
    let spec = FillerSpec {
        width: 128,
        height: 72,
        audio_codec: Some(pharos_transcode::AudioCodec::Aac),
    };
    let filler = encode_filler(td.path(), "filler_audio.m4s", spec);

    assert!(
        has_audio_stream(&filler),
        "a muxed rendition's filler must carry an audio track"
    );
    // Span from per-packet PTS, not `ffprobe stream=duration` — see
    // `stream_pts`'s doc comment; the same unreliability applies to a
    // per-stream duration field on this headerless container.
    let apts = stream_pts(&filler, "a");
    assert!(!apts.is_empty(), "the audio track must carry packets");
    // AAC @ 48 kHz: 1024 samples/frame.
    let aac_frame = 1024.0 / 48_000.0;
    let audio_span = apts.last().expect("nonempty") - apts.first().expect("nonempty") + aac_frame;
    assert!(
        (audio_span - w.duration_seconds()).abs() < aac_frame * 2.0,
        "filler audio pts span {audio_span:.6} s does not match the \
         window's {:.6} s — the silence must cover the whole window",
        w.duration_seconds()
    );
}

/// A video-only (demuxed-audio) rendition's filler carries NO audio input at
/// all — never a stray silent track a real segment of that rendition would
/// not have (which would appear as a spurious extra stream in the shared
/// fMP4 init, the exact class of defect `-map_chapters -1` above exists to
/// avoid for chapters).
#[test]
fn a_video_only_renditions_filler_carries_no_audio_track() {
    let td = tempfile::TempDir::new().unwrap();
    let spec = FillerSpec {
        width: 128,
        height: 72,
        audio_codec: None,
    };
    let filler = encode_filler(td.path(), "filler_video_only.m4s", spec);
    assert!(
        !has_audio_stream(&filler),
        "a video-only rendition's filler must not gain an audio track"
    );
}
