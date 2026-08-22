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

/// The Rogue One shape: the audio STREAM starts late in the container.
///
/// `-itsoffset` before the audio input shifts that stream's start_time, so the
/// tone generated at `TONE_FROM` sits at `TONE_FROM + delay` on the container
/// timeline — which is where a correct player puts it, and where the video at
/// that instant expects it.
///
/// Measured on the real file: Rogue One's eac3 track reports
/// `start_time 1.700000` against a video stream starting at 0.
fn make_source_with_audio_delay(dir: &Path, delay: f64) -> PathBuf {
    let src = dir.join("src_delayed.mkv");
    ffmpeg(
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=45:size=320x180:rate=24000/1001",
            "-itsoffset",
            &format!("{delay}"),
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
            "-y",
            src.to_str().unwrap(),
        ],
        "delayed-audio source synth",
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
        source_video_codec: None,
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
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    };
    let out = dir.join(format!("seg{seg}.ts"));
    let args = ffmpeg_transcode_args(
        src.to_str().unwrap(),
        &opts
            .resolve_with(|_| Ok::<_, ()>(audio.clone()))
            .expect("slice supplied")
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

/// The audio stream of a container may start LATER than the video — a real and
/// common authoring choice. Rogue One's eac3 track starts at 1.700 s.
///
/// pharos records no per-stream start time and the continuous-audio encode
/// re-stamps audio onto its own timeline, so that relationship has nowhere to
/// survive: the tone ends up under the wrong video. Reported as "the audio is
/// lagged behind the video by what feels like 500ms".
///
/// The assertion is on CONTENT, like the test above: the segment whose video
/// covers the tone's CONTAINER position must be the loud one.
#[test]
fn a_delayed_audio_stream_still_lands_under_the_right_video() {
    const DELAY: f64 = 1.7;
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source_with_audio_delay(td.path(), DELAY);
    let audio = make_continuous_audio(&src, td.path());
    let rate = frame_rate();

    // Where the tone actually is on the container timeline.
    let tone_from = TONE_FROM + DELAY;
    let tone_to = TONE_TO + DELAY;

    let mut loud = None;
    for seg in 0..8u32 {
        let (start, dur) = pharos_core::segment_range(seg, Some(rate));
        if start - pharos_transcode::DECODE_PREROLL_SECONDS < SESSION_START {
            continue;
        }
        let overlap = (start + dur).min(tone_to) - start.max(tone_from);
        if overlap > 1.0 {
            loud = Some(seg);
            break;
        }
    }
    let loud = loud.expect("a segment must overlap the delayed tone");

    let db = mean_volume_db(&encode_segment(&src, &audio, td.path(), loud));
    assert!(
        db > -40.0,
        "segment {loud} covers the tone at {tone_from}..{tone_to}s but is silent \
         ({db} dBFS) — the source's {DELAY}s audio start offset was dropped, so the \
         audio under this video came from {DELAY}s away"
    );
}

/// The FROM-ZERO continuous session — what plays at the start of a title.
///
/// `continuous_audio_args` emits neither `-ss` nor `-output_ts_offset` when the
/// session starts at 0, so nothing pins the output to the source timeline and
/// the mpegts muxer's `-avoid_negative_ts` default is free to shift the first
/// sample to zero. For a source whose audio stream starts LATE, that erases the
/// gap: every sample arrives early by the stream's start offset, from the very
/// first second of playback.
fn make_continuous_audio_from_zero(src: &Path, dir: &Path) -> MuxedAudio {
    let out = dir.join("audio0.ts");
    ffmpeg(
        &[
            "-v",
            "error",
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
            "-f",
            "mpegts",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-y",
            out.to_str().unwrap(),
        ],
        "from-zero continuous audio encode",
    );
    MuxedAudio {
        path: out,
        start_seconds: 0.0,
    }
}

/// The Rogue One report: "the audio desync started almost immediately".
///
/// The seek-session test above passes because `-ss`/`-output_ts_offset` pin
/// that encode to the source timeline. The from-0 session has no such anchor,
/// and it is the one serving the opening of every title.
#[test]
fn a_from_zero_session_keeps_a_late_audio_streams_offset() {
    const DELAY: f64 = 1.7;
    let td = tempfile::TempDir::new().unwrap();
    let src = make_source_with_audio_delay(td.path(), DELAY);
    let audio = make_continuous_audio_from_zero(&src, td.path());

    // Where the first audio sample sits on the source timeline, per the
    // continuous encode's own packets.
    let first = first_audio_pts(&audio.path);
    assert!(
        (first - DELAY).abs() < 0.2,
        "the continuous encode starts at {first:.3}s but the source's audio \
         stream starts at {DELAY}s — the offset was erased, so every sample \
         plays {:.3}s early for the whole title",
        DELAY - first
    );
}

/// PTS of the first audio packet in a file, in seconds.
fn first_audio_pts(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            "-read_intervals",
            "%+#1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .expect("a first audio pts")
}

/// The Rogue One desync, isolated — and the contract that fixes it.
///
/// `MuxedAudio::start_seconds` must be the file's TRUE first-sample time, not
/// the start its session was asked for. The segment seeks this file by
/// `input_seek - start_seconds`, and `-ss` on an input is measured FROM THAT
/// INPUT'S OWN START TIME, so any error in the field shifts the audio under the
/// video by exactly that much.
///
/// For a source whose AUDIO stream starts late, a from-0 session's first sample
/// sits at the stream's own start (1.700 s on the real title), not at 0 — so
/// recording `0.0` put every frame's audio 1.7 s late in the source. Measured
/// against the real file at the 600 s mark: our audio correlated with the
/// source at +1680 ms. Heard as audio running ahead of picture for the whole
/// title, because once a from-0 session exists every later segment reuses it.
///
/// The tone is SHORT and sits just after a segment boundary, so a 1.7 s shift
/// carries it into the PREVIOUS segment. A wide tone cannot detect this at all:
/// the first version of this test used a 2 s tone against 6 s segments and
/// passed against the bug, because the tone stayed in the same segment either
/// way.
#[test]
fn the_slice_seek_honours_the_files_true_start_not_its_requested_one() {
    const DELAY: f64 = 1.7;
    let rate = frame_rate();

    let target = 6u32;
    let (seg_start, _) = pharos_core::segment_range(target, Some(rate));
    let tone_at = seg_start + 0.30;
    let tone_end = tone_at + 0.40;
    assert!(
        tone_at - DELAY < seg_start,
        "fixture must be able to tell the two cases apart"
    );

    let td = tempfile::TempDir::new().unwrap();
    let src = make_tone_source(td.path(), DELAY, tone_at - DELAY, tone_end - DELAY);
    let session = make_continuous_audio_from_zero(&src, td.path());

    // The file really does start late — otherwise this fixture proves nothing.
    let actual = first_audio_pts(&session.path);
    assert!(
        (actual - DELAY).abs() < 0.1,
        "fixture must reproduce a late-starting session: first sample at {actual:.3}s"
    );

    // WITH the true start, the tone lands under the video it belongs to.
    let truthful = MuxedAudio {
        path: session.path.clone(),
        start_seconds: actual,
    };
    let here = mean_volume_db(&encode_segment(&src, &truthful, td.path(), target));
    let before = mean_volume_db(&encode_segment(&src, &truthful, td.path(), target - 1));
    assert!(
        here > before + 10.0,
        "with the file's true start ({actual:.3}s) the tone at {tone_at:.3}s must \
         land in segment {target}: got {here} dBFS there vs {before} dBFS in {}",
        target - 1
    );

    // WITH the requested start, it lands a whole segment early. This is the
    // shipped bug, pinned so the field's meaning cannot quietly revert.
    let naive = MuxedAudio {
        path: session.path,
        start_seconds: 0.0,
    };
    let n_here = mean_volume_db(&encode_segment(&src, &naive, td.path(), target));
    let n_before = mean_volume_db(&encode_segment(&src, &naive, td.path(), target - 1));
    assert!(
        n_before > n_here + 10.0,
        "claiming start_seconds 0.0 for a file starting at {actual:.3}s must shift \
         the audio EARLY — if this no longer holds, the slice seek changed and \
         the comment on MuxedAudio::start_seconds is stale: seg{target}={n_here} \
         dBFS seg{}={n_before} dBFS",
        target - 1
    );
}

/// Video throughout; a SHORT tone at a chosen point, with the whole audio
/// stream shifted late by `delay` so its container start_time is `delay`.
fn make_tone_source(dir: &Path, delay: f64, from: f64, to: f64) -> PathBuf {
    let src = dir.join("src_tone.mkv");
    ffmpeg(
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=60:size=320x180:rate=24000/1001",
            "-itsoffset",
            &format!("{delay}"),
            "-f",
            "lavfi",
            "-i",
            &format!("aevalsrc=sin(2*PI*1000*t)*between(t\\,{from}\\,{to}):d=60:s=48000"),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-y",
            src.to_str().unwrap(),
        ],
        "tone source synth",
    );
    src
}
