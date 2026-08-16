//! A transcoded segment carries the tracks it claims and nothing else.
//!
//! An fMP4 rung's init segment declares its tracks in one shared `moov`, and
//! hls.js appends every fragment under that declaration. A track it cannot
//! classify makes its transmux worker throw:
//!
//! ```text
//! HLS Error: Type: otherError Details: internalException Fatal: false
//!     onWorkerError ... _handleFragmentLoadProgress ...
//! ```
//!
//! and the fragment is then refetched forever — measured live as 17 requests
//! for one segment inside a single second, every one of them served `200`.
//!
//! The track that caused it was not a subtitle. `-sn` has suppressed muxed
//! subtitles since the VP9 stray-`mov_text` fix, but chapters are not an input
//! stream, so `-sn` never applied to them: ffmpeg copies a source's chapters by
//! default and the mp4 muxer writes them as a QuickTime chapter track
//! (`codec_tag=text`, `handler_name=SubtitleHandler`), which ffprobe reports as
//! a `bin_data` DATA stream. Every fMP4 rung of every chaptered source carried
//! it, video-only rungs included.
//!
//! The guard is the INVARIANT — a video-only segment contains exactly one video
//! track — rather than "no chapter track". A stray track breaks the same way
//! whichever muxer feature produced it.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and CI;
//! the test fails loudly rather than skipping, so a broken environment cannot
//! silently drop the guard.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, AudioDelivery, SegmentContainer, SegmentOpts,
    SegmentVideo,
};

const FPS_MILLE: u32 = 24_000;

fn frame_rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(FPS_MILLE).expect("24 is a valid frame rate")
}

/// A source WITH chapters — the condition the defect needed. Library sources
/// routinely carry them (a ripped episode has them by default).
fn make_chaptered_source(dir: &Path) -> PathBuf {
    let meta = dir.join("chapters.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=6000\ntitle=Cold Open\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=6000\nEND=20000\ntitle=Main\n",
    )
    .expect("write chapter metadata");

    let src = dir.join("src.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=20:size=320x180:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20:sample_rate=48000",
            "-i",
        ])
        .arg(&meta)
        .args([
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map_chapters",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libopus",
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

    // The fixture is only meaningful if the source really has chapters.
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-of", "default"])
        .arg(&src)
        .output()
        .expect("spawn ffprobe");
    let chapters = String::from_utf8_lossy(&probe.stdout)
        .matches("[CHAPTER]")
        .count();
    assert_eq!(chapters, 2, "fixture lost its chapters, guard is vacuous");
    src
}

/// One VIDEO-ONLY fMP4 segment through the REAL argv builder — the shape the
/// h264-CMAF and VP9 rungs serve, where audio is a separate rendition.
fn encode_video_only_segment(src: &Path, dir: &Path, seg: u32) -> PathBuf {
    let opts = SegmentOpts {
        source_video_codec: None,
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(2_000_000),
        window: pharos_core::SegmentWindow::for_segment(seg, Some(frame_rate()), Some(20.0)),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    };
    let out = dir.join(format!("seg{seg}.m4s"));
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
    out
}

/// `codec_type` of every stream in the produced file, in order.
fn stream_kinds(path: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn a_video_only_segment_from_a_chaptered_source_carries_one_track() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = make_chaptered_source(dir.path());

    // Segment 0 and a mid-file segment: the chapter track is written into
    // every segment's `moov`, so both must be clean.
    for seg in [0, 2] {
        let out = encode_video_only_segment(&src, dir.path(), seg);
        let kinds = stream_kinds(&out);
        assert_eq!(
            kinds,
            vec!["video".to_string()],
            "segment {seg} must carry exactly one video track, got {kinds:?} — \
             a track hls.js cannot classify makes its transmux worker throw \
             and the fragment is refetched forever"
        );
    }
}
