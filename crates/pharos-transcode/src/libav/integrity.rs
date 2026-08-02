//! Container-level integrity scan — demux the whole source, decode nothing,
//! and report what the demuxer choked on.
//!
//! This is the in-process form of `ffmpeg -v error -i FILE -c copy -f null -`,
//! the command that found three damaged episodes in one series on the live
//! deployment. It reads every packet and throws them away: no decoder is ever
//! opened, so the cost is NFS read throughput, not CPU, in the same class as
//! [`subtitle_windows`](super::subtitle_windows) which already walks an entire
//! container this way.
//!
//! # Why the whole file, not a window
//!
//! A head-only sample is worse than nothing here, because its answer for a file
//! damaged at minute 40 is "clean" — a health check whose confident pass is
//! wrong is how the damage stayed invisible in the first place. The cost is
//! bounded instead by *memoisation*: the scanner records the verdict against
//! the file's `(mtime, size)` and never reads it again until it changes.
//!
//! # Termination
//!
//! `PacketIter` yields `Some(Err(..))` and continues for every error that is
//! not EOF, so a demuxer stuck on an unparseable byte would return the same
//! error forever without advancing the file position. Two bounds
//! ([`MAX_READ_ERRORS`], [`MAX_CONSECUTIVE_READ_ERRORS`]) stop the scan once
//! the verdict can no longer change — reading further would only re-learn what
//! is already known, while holding a background-I/O permit.

use super::frames::FrameError;
use crate::integrity::{IntegrityReport, MAX_CONSECUTIVE_READ_ERRORS, MAX_READ_ERRORS};
use ffmpeg_the_third as ffmpeg;

use std::path::Path;

/// Demux `src` end to end without decoding and report container faults.
///
/// A file whose *header* cannot be opened is an `Err`, not a damaged report:
/// that is the failure the probe already detects and memoises, and folding the
/// two together would make an unreadable file indistinguishable from a
/// readable-but-rotten one.
pub fn scan_integrity(src: &Path) -> Result<IntegrityReport, FrameError> {
    super::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let mut ictx =
        ffmpeg::format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;

    // Per-stream time base, indexed by stream index, so a packet's pts can be
    // turned into a wall position without re-walking the stream list per
    // packet.
    let time_bases: Vec<(i32, i32)> = {
        let n = ictx.streams().count();
        let mut v = vec![(1, 1000); n];
        for stream in ictx.streams() {
            let tb = stream.time_base();
            if let Some(slot) = v.get_mut(stream.index()) {
                *slot = (tb.numerator(), tb.denominator());
            }
        }
        v
    };

    // Read before demuxing: this is the header's promise, against which the
    // body is judged.
    let declared_ms = {
        let d = ictx.duration();
        (d > 0).then(|| (d as i128 * 1000 / ffmpeg::ffi::AV_TIME_BASE as i128) as u64)
    };

    let capture = super::logbridge::ErrorCapture::open();
    let mut report = IntegrityReport {
        declared_ms,
        ..Default::default()
    };
    let mut consecutive_errors: u32 = 0;
    // Errors already accounted for, so a rise can be attributed to the media
    // position the scan had reached when it happened.
    let mut logged_seen: u64 = 0;
    let mut first_logged_ms: Option<u64> = None;

    for res in ictx.packets() {
        match res {
            Ok((stream, packet)) => {
                consecutive_errors = 0;
                report.packets += 1;

                let pos_ms = packet
                    .pts()
                    .or_else(|| packet.dts())
                    .and_then(|ts| {
                        time_bases
                            .get(stream.index())
                            .map(|&(num, den)| to_ms(ts, num, den))
                    })
                    .unwrap_or(report.scanned_ms);
                report.scanned_ms = report.scanned_ms.max(pos_ms);

                if packet.is_corrupt() {
                    report.corrupt_packets += 1;
                    note_fault(&mut report, pos_ms, "packet flagged corrupt");
                }

                // A logged fault carries no timestamp of its own. The furthest
                // position demuxed when the count rose is a lower bound on
                // where it is in the media — approximate, and far more use to
                // someone deciding whether to re-acquire a file than nothing.
                let logged = capture.count();
                if logged > logged_seen {
                    logged_seen = logged;
                    first_logged_ms.get_or_insert(pos_ms);
                }
            }
            Err(e) => {
                report.read_errors += 1;
                consecutive_errors += 1;
                let at = report.scanned_ms;
                note_fault(&mut report, at, &e.to_string());
                if report.read_errors >= MAX_READ_ERRORS
                    || consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS
                {
                    finish(&mut report, &capture, first_logged_ms);
                    return Ok(report);
                }
            }
        }
    }

    report.complete = true;
    finish(&mut report, &capture, first_logged_ms);
    Ok(report)
}

/// Fold what libav *said* into the report alongside what it *returned*.
fn finish(
    report: &mut IntegrityReport,
    capture: &super::logbridge::ErrorCapture,
    first_logged_ms: Option<u64>,
) {
    let (count, first) = capture.errors();
    report.demux_errors = count;
    if count > 0 && report.first_fault.is_none() {
        report.first_fault_ms = first_logged_ms.or(Some(0));
        report.first_fault = first.or_else(|| Some("libav demuxer error".into()));
    }
}

/// Record the FIRST fault only. Later ones are almost always the same damage
/// re-reported, and overwriting would move the position an operator uses to
/// decide whether a file is worth re-acquiring.
fn note_fault(report: &mut IntegrityReport, pos_ms: u64, why: &str) {
    if report.first_fault.is_none() {
        report.first_fault_ms = Some(pos_ms);
        report.first_fault = Some(why.to_owned());
    }
}

fn to_ms(ts: i64, num: i32, den: i32) -> u64 {
    if ts <= 0 || den == 0 {
        return 0;
    }
    ((ts as i128 * num as i128 * 1000) / den as i128).max(0) as u64
}
