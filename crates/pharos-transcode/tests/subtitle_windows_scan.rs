//! B46 — behavioral guard on the subtitle event-window scan that drives
//! per-segment burn gating. Synthesises a real MKV with a subtitle track
//! at known cue times, runs the in-process libav scan, and asserts the
//! recovered windows cover the cues — so a libav packet-iteration
//! regression (wrong stream, wrong timebase, dropped packets) can't
//! silently disable or mis-aim gating.
//!
//! The track is SubRip, not PGS: the scan reads the PACKET timeline and is
//! codec-agnostic, and ffmpeg ships no text→bitmap subtitle encoder to
//! synthesise a real PGS fixture ("Subtitle encoding currently only
//! possible from text to text or bitmap to bitmap"). Production gating
//! only ever runs on image tracks (the burn guard upstream filters), but
//! the demux path under test is identical.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::subwin::any_window_overlaps;
use std::process::Command;

/// Two cues: 2-4 s and 14-16 s, inside a 20 s clip.
fn make_source(dir: &std::path::Path) -> std::path::PathBuf {
    let srt = dir.join("cues.srt");
    std::fs::write(
        &srt,
        "1\n00:00:02,000 --> 00:00:04,000\nfirst cue\n\n2\n00:00:14,000 --> 00:00:16,000\nsecond cue\n",
    )
    .unwrap();
    let out = dir.join("src.mkv");
    let st = Command::new("ffmpeg")
        .args(["-v", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=20:size=320x180:rate=25",
        ])
        .args(["-i"])
        .arg(&srt)
        .args(["-map", "0:v", "-map", "1:s"])
        .args(["-c:v", "libx264", "-preset", "ultrafast"])
        .args(["-c:s", "srt", "-y"])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg (run inside the devShell)");
    assert!(st.success(), "fixture synth failed");
    out
}

#[test]
fn scan_recovers_image_sub_event_windows() {
    let dir = tempfile::tempdir().unwrap();
    let src = make_source(dir.path());

    let windows =
        pharos_transcode::libav::subtitle_windows::subtitle_event_windows(&src, 0).expect("scan");

    // Both cues recovered…
    assert!(
        any_window_overlaps(&windows, 2_500, 3_500),
        "first cue (2-4 s) not covered: {windows:?}"
    );
    assert!(
        any_window_overlaps(&windows, 14_500, 15_500),
        "second cue (14-16 s) not covered: {windows:?}"
    );
    // …and the silent stretches stay silent — this is the property gating
    // relies on to SKIP burns (a scan that smears windows over everything
    // would silently disable the optimization).
    assert!(
        !any_window_overlaps(&windows, 6_000, 12_000),
        "gap 6-12 s wrongly covered: {windows:?}"
    );
    assert!(
        !any_window_overlaps(&windows, 18_500, 20_000),
        "tail 18.5-20 s wrongly covered: {windows:?}"
    );

    // A rel-index past the only subtitle stream errors instead of
    // returning an empty timeline (empty = "never burn", which would LOSE
    // subtitles if an index bug mapped a real track here).
    assert!(
        pharos_transcode::libav::subtitle_windows::subtitle_event_windows(&src, 1).is_err(),
        "nonexistent track must be an error, not an empty timeline"
    );
}
