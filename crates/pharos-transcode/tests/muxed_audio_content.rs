//! A segment's muxed audio must be the audio that belongs under its video.
//!
//! The audio is copied from the title's one continuous encode, which for a
//! seek is a file that starts partway into the source. `-ss` on an input is
//! measured from THAT input's own start time, so seeking such a file by an
//! absolute source position lands at `session_start + position`. Measured on
//! ffmpeg 8.1 with a tone-marked fixture — a file spanning source 30..40 s
//! with a tone at 32..34 s:
//!
//! ```text
//! -ss 2   (file-relative)  -> mean_volume -6.6 dB   the tone
//! -ss 32  (absolute)       -> mean_volume -91.0 dB  silence, seeked past EOF
//! ```
//!
//! That shipped. The video was correct and the audio came from somewhere else
//! entirely, reported as "playing music while someone is visibly talking".
//!
//! `segment_audio_continuity.rs` did not catch it because its fixture used a
//! whole-title continuous encode, whose start time is 0 — the one case where
//! relative and absolute seeking agree. This test uses a SEEK session, which
//! is what production uses for anything past the first few minutes.
//!
//! The assertion is on CONTENT, not timestamps: with the bug the timestamps
//! are still correct, because each input is re-based by its own seek. Only
//! the audio underneath them is wrong.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH — guaranteed inside the devShell and
//! CI; the test fails loudly rather than skipping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_transcode::{
    ffmpeg_transcode_args, protocol::DeviceId, AudioDelivery, ContinuousAudio, MuxedAudio,
    SegmentAudio, SegmentContainer, SegmentOpts, SegmentVideo,
};

const FPS_MILLE: u32 = 23_976;
/// The only stretch of the source that carries any sound.
const TONE_FROM: f64 = 32.0;
const TONE_TO: f64 = 34.0;
/// Where the continuous encode starts. Must be at or before the segment's
/// input seek (one decode preroll before its start), which is what the cache
/// guarantees when it picks a session.
const SESSION_START: f64 = 12.0;

fn frame_rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(FPS_MILLE).expect("23.976 is a valid frame rate")
}

fn ffmpeg(args: &[&str], what: &str) {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .expect("spawn ffmpeg (is it on PATH? run inside the devShell)");
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Video throughout, audio ONLY between `TONE_FROM` and `TONE_TO`. A segment
/// that takes its audio from the wrong place is then audible as silence, and
/// one that takes it from the right place is audible as the tone.
fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    ffmpeg(
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=45:size=320x180:rate=24000/1001",
            "-f",
            "lavfi",
            "-i",
            &format!("aevalsrc=sin(2*PI*1000*t)*between(t\\,{TONE_FROM}\\,{TONE_TO}):d=45:s=48000"),
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
            src.to_str().unwrap(),
        ],
        "source synth",
    );
    src
}

/// A SEEK session of the continuous encode: starts partway into the source
/// and carries absolute PTS, exactly as `HlsSegmentCache` produces it.
fn make_continuous_audio(src: &Path, dir: &Path) -> MuxedAudio {
    let out = dir.join("audio.ts");
    ffmpeg(
        &[
            "-v",
            "error",
            "-ss",
            &format!("{SESSION_START:.6}"),
            "-i",
            src.to_str().unwrap(),
            "-vn",
            "-map",
            "0:a:0",
            "-c:a",
            "aac",
            "-b:a",
            "128000",
            "-ac",
            "2",
            "-output_ts_offset",
            &format!("{SESSION_START:.6}"),
            "-f",
            "mpegts",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-y",
            out.to_str().unwrap(),
        ],
        "continuous audio encode",
    );
    MuxedAudio {
        path: out,
        start_seconds: SESSION_START,
    }
}

fn encode_segment(src: &Path, audio: &MuxedAudio, dir: &Path, seg: u32) -> PathBuf {
    let (_start, _dur) = pharos_core::segment_range(seg, Some(frame_rate()));
    let opts = SegmentOpts {
        container: SegmentContainer::Mpegts,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Muxed(ContinuousAudio {
            codec: SegmentAudio::Aac,

            bitrate_bps: Some(128_000),
        }),

        video_bitrate_bps: Some(2_000_000),
        window: pharos_core::SegmentWindow::for_segment(seg, Some(frame_rate()), Some(45.0)),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        muxed_audio_source: Some(audio.clone()),
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

/// Mean volume of a file in dBFS. Digital silence reports `-inf`, which
/// parses to negative infinity and compares correctly.
fn mean_volume_db(path: &Path) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-i"])
        .arg(path)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg");
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err
        .lines()
        .find(|l| l.contains("mean_volume:"))
        .unwrap_or_else(|| panic!("no volumedetect output for {}:\n{err}", path.display()));
    let raw = line
        .split("mean_volume:")
        .nth(1)
        .unwrap()
        .trim()
        .trim_end_matches(" dB")
        .trim();
    if raw.contains("inf") {
        return f64::NEG_INFINITY;
    }
    raw.parse()
        .unwrap_or_else(|e| panic!("bad volume {raw:?}: {e}"))
}

#[test]
fn a_segment_carries_the_audio_that_belongs_under_its_video() {
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source(td.path());
    let audio = make_continuous_audio(&src, td.path());
    let rate = frame_rate();

    // Find the segments that do and do not overlap the tone, from the grid
    // rather than by hand — the boundaries are frame-snapped and not round.
    let mut loud = None;
    let mut quiet = None;
    for seg in 0..7u32 {
        let (start, dur) = pharos_core::segment_range(seg, Some(rate));
        // Only segments this session can actually serve: the segment seeks
        // both inputs to one decode preroll before its start, so the session
        // must begin at or before that point. The cache guarantees this by
        // choosing the session start from the segment; a fixed session here
        // has to skip the segments it would not have been chosen for.
        if start - pharos_transcode::DECODE_PREROLL_SECONDS < SESSION_START {
            continue;
        }
        let covers = start < TONE_TO && start + dur > TONE_FROM;
        // Require a decent overlap so the mean is not diluted by silence.
        let overlap = (start + dur).min(TONE_TO) - start.max(TONE_FROM);
        if covers && overlap > 1.5 && loud.is_none() {
            loud = Some(seg);
        } else if !covers && quiet.is_none() {
            quiet = Some(seg);
        }
    }
    let loud = loud.expect("a segment overlapping the tone");
    let quiet = quiet.expect("a segment clear of the tone");

    let loud_db = mean_volume_db(&encode_segment(&src, &audio, td.path(), loud));
    let quiet_db = mean_volume_db(&encode_segment(&src, &audio, td.path(), quiet));

    // The decisive assertion. With the audio taken from the wrong position
    // this segment is silent (the seek lands past the end of the continuous
    // encode) while its video is perfectly correct.
    assert!(
        loud_db > -30.0,
        "segment {loud} covers the tone at {TONE_FROM}-{TONE_TO}s but its \
         audio is {loud_db} dBFS — it was copied from the wrong position in \
         the continuous encode (which starts at {SESSION_START}s, so the \
         segment must seek it RELATIVELY)"
    );
    assert!(
        quiet_db < loud_db - 20.0,
        "segment {quiet} does not overlap the tone yet carries {quiet_db} \
         dBFS against the tone segment's {loud_db} — its audio came from \
         somewhere else in the title"
    );
}
