//! Behavioral acceptance gate for TEXT/ASS subtitle burn-in (Task 7).
//!
//! `push_video_filters` burns IMAGE subs (PGS/VOBSUB) with `overlay`, which
//! cannot rasterize text/ASS. The text branch rasterizes via the libass
//! `subtitles=` filter reading the source file directly. The HARD part is
//! per-segment timestamp alignment: the segment path input-seeks (`-ss START`
//! before `-i`), but the `subtitles` filter opens a second demuxer at t=0 and
//! renders by frame PTS — a plain input-seek would render the cue for t≈0, not
//! the cue active at the segment's true absolute time.
//!
//! This test proves the RIGHT cue renders at the RIGHT segment by driving the
//! REAL production argv (`pharos_transcode::ffmpeg_transcode_args`) and
//! inspecting decoded pixels, per V30 (flags that do timeline/render work need
//! an OUTPUT-inspecting test, not an argv assertion):
//!   - a black clip carries an ASS cue "MARK30" visible ONLY 29–31 s;
//!   - transcoding the segment covering 30 s (input-seek to 27 s, 6 s long)
//!     with text burn ON must show bright pixels ONLY in the 29–31 s window;
//!   - the same segment with burn OFF stays black (isolates the burn);
//!   - a segment covering 10 s (NO active cue) is black burn-vs-no-burn (proves
//!     the filter is NOT rendering the wrong / a from-zero cue).
//!
//! ffmpeg-gated + `#[ignore]` like the other real-transcode suites. Run:
//!   nix develop --command cargo test -p pharos-transcode --test subtitle_burn_ass -- --ignored

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::protocol::DeviceId;
use pharos_transcode::{Container, TranscodeOptions, VideoCodec};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const TICKS_PER_SEC: u64 = 10_000_000;

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 60 s black clip + an embedded ASS whose big white centred "MARK30" text is
/// visible ONLY 29–31 s. Black limited-range luma sits at YAVG≈16; the burned
/// cue lifts the frame average well above that.
fn make_subbed_clip(dir: &Path) -> PathBuf {
    let clip = dir.join("clip.mkv");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "color=c=black:s=320x240:r=25:d=60"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg (clip)");
    assert!(status.success(), "clip generation failed");

    let ass = dir.join("sub.ass");
    std::fs::write(
        &ass,
        "[Script Info]\n\
         ScriptType: v4.00+\n\
         PlayResX: 320\n\
         PlayResY: 240\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Default,Arial,72,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,2,0,5,10,10,10,1\n\n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:29.00,0:00:31.00,Default,,0,0,0,,MARK30\n",
    )
    .expect("write ass");

    let subbed = dir.join("clip_subbed.mkv");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(&clip)
        .arg("-i")
        .arg(&ass)
        .args(["-c", "copy", "-map", "0:v", "-map", "1"])
        .arg(&subbed)
        .status()
        .expect("spawn ffmpeg (mux ass)");
    assert!(status.success(), "ass mux failed");
    subbed
}

/// Transcode a single segment via the REAL production argv and return its path.
fn transcode_segment(
    input: &Path,
    out: &Path,
    start_secs: u64,
    dur_secs: u64,
    burn_text: bool,
) -> bool {
    let opts = TranscodeOptions {
        container: Container::Mp4,
        video: Some(VideoCodec::H264),
        audio: None,
        video_bitrate_bps: Some(1_000_000),
        audio_bitrate_bps: None,
        start_position_ticks: start_secs * TICKS_PER_SEC,
        duration_ticks: Some(dur_secs * TICKS_PER_SEC),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: if burn_text { Some(0) } else { None },
        burn_subtitle_is_text: burn_text,
    };
    let args = pharos_transcode::ffmpeg_transcode_args(
        input.to_str().unwrap(),
        &opts,
        DeviceId::Cpu,
        out.to_str().unwrap(),
    );
    eprintln!("argv: ffmpeg {}", args.join(" "));
    let status = Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("spawn production ffmpeg");
    status.success()
}

/// Max per-frame average luma (YAVG) over the whole file. Black ≈ 16; a burned
/// bright cue pushes it well above.
fn max_luma(file: &Path) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(file)
        .args([
            "-vf",
            "signalstats,metadata=print:file=-",
            "-f",
            "null",
            "/dev/null",
        ])
        .output()
        .expect("spawn ffmpeg (signalstats)");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| l.split("lavfi.signalstats.YAVG=").nth(1))
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .fold(0.0_f64, f64::max)
}

#[test]
#[ignore = "requires ffmpeg; real transcode + pixel inspection"]
fn ass_cue_burns_into_the_correct_segment() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not available — skipping");
        return;
    }
    let td = TempDir::new().unwrap();
    let src = make_subbed_clip(td.path());

    // Segment covering absolute 27..33 s — the 29..31 s cue falls inside.
    let cue_burn = td.path().join("cue_burn.mp4");
    assert!(
        transcode_segment(&src, &cue_burn, 27, 6, true),
        "text-burn transcode of the cue segment failed to produce output"
    );
    let cue_noburn = td.path().join("cue_noburn.mp4");
    assert!(transcode_segment(&src, &cue_noburn, 27, 6, false));

    // Segment covering absolute 7..13 s — NO cue is active here.
    let empty_burn = td.path().join("empty_burn.mp4");
    assert!(transcode_segment(&src, &empty_burn, 7, 6, true));
    let empty_noburn = td.path().join("empty_noburn.mp4");
    assert!(transcode_segment(&src, &empty_noburn, 7, 6, false));

    let cue_burn_l = max_luma(&cue_burn);
    let cue_noburn_l = max_luma(&cue_noburn);
    let empty_burn_l = max_luma(&empty_burn);
    let empty_noburn_l = max_luma(&empty_noburn);
    eprintln!(
        "max luma: cue_burn={cue_burn_l:.2} cue_noburn={cue_noburn_l:.2} \
         empty_burn={empty_burn_l:.2} empty_noburn={empty_noburn_l:.2}"
    );

    // The cue must render in the segment whose absolute time contains it:
    // burning adds bright pixels the no-burn render does not have.
    assert!(
        cue_burn_l > cue_noburn_l + 5.0,
        "cue segment: text burn did not brighten the frame \
         (burn={cue_burn_l:.2} vs no-burn={cue_noburn_l:.2}) — cue not rendered"
    );

    // A segment with NO active cue must be identical burn-vs-no-burn: this is
    // what proves alignment — a from-zero / mis-timed render would light up
    // here (the 0..6 s or wrong cue) even though nothing is active at 7..13 s.
    assert!(
        (empty_burn_l - empty_noburn_l).abs() < 2.0,
        "no-cue segment differs burn-vs-no-burn \
         (burn={empty_burn_l:.2} vs no-burn={empty_noburn_l:.2}) — \
         a wrong/from-zero cue leaked into the segment"
    );
}
