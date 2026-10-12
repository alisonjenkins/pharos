//! Container-integrity scan against real media: an intact H.264 MKV must come
//! back clean, and the same file with a hole punched in its middle must come
//! back damaged, naming where.
//!
//! The fixture is corrupted by zeroing a run of bytes mid-file, which is the
//! exact shape of the damage found on the live deployment — three of one
//! series' 26 episodes are real-size files with zeroed regions, and every one
//! of them probes perfectly. Synthesising the damage rather than asserting on a
//! hand-built report is deliberate: the thing under test is whether libav's
//! demuxer surfaces the fault at all, which a mocked packet stream assumes.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 20 s of H.264 in Matroska — long enough that a mid-file hole lands well
/// past the header, so the file still opens and still probes.
fn synth_mkv(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=20:size=320x240:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "25",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

/// Zero `len` bytes starting at the midpoint of the file.
fn punch_hole(path: &Path, len: usize) {
    let size = std::fs::metadata(path).expect("stat fixture").len();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open fixture");
    f.seek(SeekFrom::Start(size / 2)).expect("seek");
    f.write_all(&vec![0u8; len]).expect("write hole");
    f.sync_all().expect("sync");
}

#[test]
fn an_intact_container_scans_clean() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("good.mkv");
    synth_mkv(&input);

    let report = pharos_transcode::libav::integrity::scan_integrity(&input).expect("scan ok");
    assert!(
        !report.is_damaged(),
        "an intact file must not be reported damaged: {}",
        report.summary()
    );
    assert!(report.complete, "the scan must reach EOF: {report:?}");
    assert!(report.packets > 0, "no packets demuxed: {report:?}");
    assert_eq!(report.label(), "clean");
    // 20 s of media, allowing container slack at the tail.
    assert!(
        report.scanned_ms >= 19_000,
        "scanned_ms = {} — the scan stopped short of the end",
        report.scanned_ms
    );
}

/// The load-bearing test, and it has already earned its keep: the first
/// implementation of `scan_integrity` counted only API-level faults —
/// `av_read_frame` errors and `AV_PKT_FLAG_CORRUPT` — and this test failed
/// against exactly this fixture with `read_errors: 0, corrupt_packets: 0,
/// complete: true`. The matroska demuxer resyncs past the hole in silence. Take
/// `ErrorCapture` back out of `scan_integrity` and it returns to that state.
#[test]
fn a_hole_punched_mid_file_is_reported_with_its_position() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("holed.mkv");
    synth_mkv(&input);

    // Precondition: this file is healthy before the hole. Without this the
    // test could pass on a fixture that was broken from the start.
    let before = pharos_transcode::libav::integrity::scan_integrity(&input).expect("scan ok");
    assert!(!before.is_damaged(), "fixture was damaged before the hole");

    punch_hole(&input, 64 * 1024);

    // The damage is invisible to the probe — this is the whole reason the
    // integrity scan has to exist.
    let probe = pharos_transcode::libav::probe::probe(&input);
    assert!(
        probe.is_ok(),
        "the probe is expected to still succeed on a mid-file hole; if it \
         fails, this fixture no longer demonstrates the gap the scan fills"
    );

    let report = pharos_transcode::libav::integrity::scan_integrity(&input).expect("scan ok");
    assert!(
        report.is_damaged(),
        "a 64 KiB hole must be reported: {report:?}"
    );
    assert!(
        report.first_fault.is_some(),
        "the demuxer's own words must be carried, not a bare class: {report:?}"
    );
    assert!(
        report.first_fault_ms.is_some(),
        "the position of the first fault is what decides whether a file is \
         worth re-acquiring: {report:?}"
    );
    assert!(
        matches!(report.label(), "damaged" | "unreadable"),
        "label = {}",
        report.label()
    );
}

/// A demuxer wedged on an unparseable byte returns the same error forever
/// without advancing. The bounds must stop it; without them the op runs until
/// the worker's heavy-op timeout and reports a hung worker rather than a
/// damaged file.
#[test]
fn a_wholly_shredded_body_terminates_rather_than_spinning() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("shredded.mkv");
    synth_mkv(&input);

    // Zero everything after the header: the worst case for a demuxer that
    // keeps being asked for another packet.
    let size = std::fs::metadata(&input).expect("stat").len();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&input)
        .expect("open");
    f.seek(SeekFrom::Start(4096)).expect("seek");
    f.write_all(&vec![0u8; (size - 4096) as usize])
        .expect("write");
    f.sync_all().expect("sync");
    drop(f);

    let started = std::time::Instant::now();
    let report = pharos_transcode::libav::integrity::scan_integrity(&input);
    let elapsed = started.elapsed();

    // Either it opens and reports damage, or the header itself is gone and it
    // is a plain open failure. Both are terminal; spinning is not.
    //
    // This is the case libav reports as SUCCESS: it returns one packet, then a
    // clean end-of-file, with no error and no flag. Only the shortfall against
    // the header's declared duration catches it.
    if let Ok(r) = report {
        assert!(r.is_damaged(), "a shredded body must not scan clean: {r:?}");
    }
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "scan took {elapsed:?} — the read-error bounds are not stopping it"
    );
}
