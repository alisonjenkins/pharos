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

    // Positions are reported on the player's timeline, which starts at the
    // container's first timestamp. MPEG-TS rips rarely start at 0, and a span
    // left in raw pts would be marked minutes away from the damage.
    let origin_ms = {
        // SAFETY: `ictx` owns a live, opened format context.
        let st = unsafe { (*ictx.as_ptr()).start_time };
        if st == ffmpeg::ffi::AV_NOPTS_VALUE || st <= 0 {
            0
        } else {
            (st as i128 * 1000 / ffmpeg::ffi::AV_TIME_BASE as i128) as u64
        }
    };

    // Before the first packet: the AVI demuxer adds an index entry for every
    // packet it reads from an unindexed file, so after the walk below every
    // AVI would look indexed.
    let seek_index = avi_seek_index(&ictx, declared_ms, origin_ms);

    let capture = super::logbridge::ErrorCapture::open();
    let mut report = IntegrityReport {
        declared_ms,
        ranges: Some(Vec::new()),
        seek_index,
        ..Default::default()
    };
    // Set from a fault until the next packet that reads cleanly, which is
    // where that span of damage ends.
    let mut in_fault = false;
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
                            .map(|&(num, den)| to_ms(ts, num, den).saturating_sub(origin_ms))
                    })
                    .unwrap_or(report.scanned_ms);
                // The furthest position before this packet: the last media
                // known to be intact.
                let before = report.scanned_ms;
                report.scanned_ms = report.scanned_ms.max(pos_ms);

                if in_fault {
                    report.close_range(pos_ms);
                }
                in_fault = false;

                // A logged fault carries no timestamp of its own. It was
                // logged while the demuxer resynced to reach this packet, so
                // the media between the last packet and this one is what it
                // skipped: a matroska hole at 10 s can resync at 19 s.
                let logged = capture.count();
                if logged > logged_seen {
                    logged_seen = logged;
                    first_logged_ms.get_or_insert(before);
                    report.note_range(before);
                    report.close_range(pos_ms);
                }

                if packet.is_corrupt() {
                    report.corrupt_packets += 1;
                    note_fault(&mut report, pos_ms, "packet flagged corrupt");
                    report.note_range(pos_ms);
                    in_fault = true;
                }
            }
            Err(e) => {
                report.read_errors += 1;
                consecutive_errors += 1;
                let at = report.scanned_ms;
                note_fault(&mut report, at, &e.to_string());
                report.note_range(at);
                in_fault = true;
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

/// Share of the declared duration an AVI's index must reach before a seek
/// can use it. An indexed AVI's `idx1` or OpenDML `indx` lists every chunk to
/// the end. An unindexed one holds only the entries the demuxer added for
/// the few packets `avformat_find_stream_info` read, a second or two, so
/// anything between those two shapes is far from the threshold.
const MIN_INDEX_COVERAGE: f64 = 0.5;

/// Whether an AVI's index reaches far enough for a seek to use it (B1162,
/// V1122). `None` for every other container: Matroska reads its cues only when
/// asked to seek, and MP4's sample tables are its header, so an entry count
/// taken at open says nothing about them. `None` too when the header states
/// no duration to measure coverage against.
fn avi_seek_index(
    ictx: &ffmpeg::format::context::Input,
    declared_ms: Option<u64>,
    origin_ms: u64,
) -> Option<bool> {
    // SAFETY: `ictx` owns a live, opened format context, and `iformat` is set
    // for every opened input.
    let name = unsafe {
        let iformat = (*ictx.as_ptr()).iformat;
        if iformat.is_null() || (*iformat).name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*iformat).name)
            .to_string_lossy()
            .into_owned()
    };
    if name != "avi" {
        return None;
    }
    let declared = declared_ms.filter(|d| *d > 0)?;
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .or_else(|| ictx.streams().next())?;
    let tb = stream.time_base();
    // SAFETY: the stream belongs to `ictx`, which is alive; the entry pointer
    // is read at once, before anything can add to the index.
    let last_ms = unsafe {
        let st = stream.as_ptr();
        let n = ffmpeg::ffi::avformat_index_get_entries_count(st);
        if n <= 0 {
            return Some(false);
        }
        let e = ffmpeg::ffi::avformat_index_get_entry(st as *mut _, n - 1);
        if e.is_null() {
            return Some(false);
        }
        to_ms((*e).timestamp, tb.numerator(), tb.denominator()).saturating_sub(origin_ms)
    };
    Some(last_ms as f64 >= declared as f64 * MIN_INDEX_COVERAGE)
}

/// Fold what libav *said* into the report alongside what it *returned*.
fn finish(
    report: &mut IntegrityReport,
    capture: &super::logbridge::ErrorCapture,
    first_logged_ms: Option<u64>,
) {
    if report.is_short() || !report.complete {
        let end = report.declared_ms.unwrap_or(report.scanned_ms);
        report.note_range(report.scanned_ms);
        report.close_range(end);
    }
    let (count, first) = capture.errors();
    report.demux_errors = count;
    // An error logged after the last packet never reached the per-packet
    // check. It belongs where reading stopped, not at the start of the file.
    if count > 0 && report.ranges.as_ref().is_some_and(Vec::is_empty) {
        report.note_range(first_logged_ms.unwrap_or(report.scanned_ms));
    }
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
