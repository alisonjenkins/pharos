//! What a container-level integrity scan found, and how to say it in one word.
//!
//! A file can probe perfectly and still be broken. `avformat_open_input` reads
//! the header — codec, resolution, duration — and a file whose header is intact
//! but whose middle is a hole of null bytes answers every question the scanner
//! asks. It enters the library looking healthy, and the damage is discovered by
//! a viewer, mid-episode, as a player that stops or a client that crashes.
//!
//! That is not hypothetical: three of the 26 episodes of one series on the live
//! deployment carry exactly this shape, and the first anyone knew of it was the
//! Google TV app dying 2.8 s into S01E25. The bytes were unreadable at scan
//! time; nothing looked.
//!
//! This type is the record of having looked. It is deliberately more than a
//! boolean: an operator deciding whether to re-acquire a file needs to know
//! *where* the damage starts and what the demuxer called it — "damaged" alone
//! is another round of guessing (§"Expose the cause").

use serde::{Deserialize, Serialize};

/// The outcome of demuxing a source end to end without decoding it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// Packets successfully demuxed.
    pub packets: u64,
    /// Packets the demuxer itself flagged `AV_PKT_FLAG_CORRUPT`.
    pub corrupt_packets: u64,
    /// `av_read_frame` failures that were not end-of-file.
    pub read_errors: u64,
    /// `AV_LOG_ERROR`-and-worse lines libav emitted during the scan.
    ///
    /// This is the signal that actually finds a hole punched in a Matroska
    /// body: the demuxer resyncs past it internally, so no API call fails and
    /// no packet is flagged, but it says so in the log. `ffmpeg -v error … -c
    /// copy -f null -` is exactly this count being non-zero.
    pub demux_errors: u64,
    /// Media position of the first fault, in ms — the timestamp a viewer
    /// would reach before playback breaks.
    pub first_fault_ms: Option<u64>,
    /// The demuxer's own words for the first fault, e.g. `Invalid data found
    /// when processing input`. Kept verbatim: a reason that does not name the
    /// value is another round of guessing.
    pub first_fault: Option<String>,
    /// Furthest media position reached, in ms.
    pub scanned_ms: u64,
    /// Duration the container's header claims, in ms, when it states one.
    ///
    /// The third failure shape, and the one neither errors nor flags catch: a
    /// body that is simply *absent*. Zero the packets of a 20 s file and libav
    /// reports no error at all — it returns one packet and then end-of-file,
    /// cleanly. Only the header's own claim reveals that 20 s of media went
    /// missing.
    pub declared_ms: Option<u64>,
    /// Whether the scan ran to end-of-file. `false` means it gave up early
    /// under [`MAX_READ_ERRORS`] or [`MAX_CONSECUTIVE_READ_ERRORS`] — the file
    /// is damaged past the point where reading more of it tells us anything.
    pub complete: bool,
}

/// Total non-EOF read errors after which the scan stops.
///
/// Past this the verdict cannot change, and every further packet is another
/// NFS read holding a background-I/O permit to re-learn what we already know.
pub const MAX_READ_ERRORS: u64 = 256;

/// Consecutive non-EOF read errors after which the scan stops.
///
/// `PacketIter` yields `Some(Err(..))` and keeps going for any error that is
/// not EOF, so a demuxer wedged on an unparseable byte returns the same error
/// forever without advancing. Without this bound the op does not terminate; it
/// runs until the worker's heavy-op timeout kills it, which reports a hung
/// worker rather than a damaged file.
pub const MAX_CONSECUTIVE_READ_ERRORS: u32 = 32;

/// Fraction of the declared duration that may go undemuxed before the body is
/// called short. Container durations are exact for mp4/matroska and estimated
/// for some streaming containers, so the margin is deliberately wide: this is
/// meant to catch a body that is *gone*, not one that ends a little early.
pub const MAX_SHORTFALL_RATIO: f64 = 0.25;

/// Absolute floor on the same test, so a short clip cannot trip it on a
/// rounding difference.
pub const MIN_SHORTFALL_MS: u64 = 5_000;

impl IntegrityReport {
    /// How much of the declared duration was never demuxed, in ms.
    pub fn shortfall_ms(&self) -> u64 {
        self.declared_ms
            .unwrap_or(0)
            .saturating_sub(self.scanned_ms)
    }

    /// Whether the body stops well short of what the header promises.
    pub fn is_short(&self) -> bool {
        let Some(declared) = self.declared_ms.filter(|d| *d > 0) else {
            return false;
        };
        let short = self.shortfall_ms();
        short >= MIN_SHORTFALL_MS && (short as f64 / declared as f64) > MAX_SHORTFALL_RATIO
    }

    /// Whether anything was wrong with the container.
    pub fn is_damaged(&self) -> bool {
        self.corrupt_packets > 0 || self.read_errors > 0 || self.demux_errors > 0 || self.is_short()
    }

    /// Bounded, stable metric label. A dashboard contract — renaming one of
    /// these breaks an alert silently.
    pub fn label(&self) -> &'static str {
        if !self.is_damaged() {
            "clean"
        } else if !self.complete {
            "unreadable"
        } else if self.corrupt_packets > 0 || self.read_errors > 0 || self.demux_errors > 0 {
            "damaged"
        } else {
            "short"
        }
    }

    /// One line for the scan log, naming the position and the cause.
    pub fn summary(&self) -> String {
        if !self.is_damaged() {
            return format!("clean ({} packets, {} ms)", self.packets, self.scanned_ms);
        }
        if self.label() == "short" {
            return format!(
                "short: demuxed {} ms of a declared {} ms ({} packets)",
                self.scanned_ms,
                self.declared_ms.unwrap_or(0),
                self.packets,
            );
        }
        format!(
            "{} at {} ms ({} demuxer errors, {} corrupt packets, {} read errors): {}",
            self.label(),
            self.first_fault_ms.unwrap_or(self.scanned_ms),
            self.demux_errors,
            self.corrupt_packets,
            self.read_errors,
            self.first_fault.as_deref().unwrap_or("unspecified"),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_undamaged_scan_is_clean() {
        let r = IntegrityReport {
            packets: 500,
            scanned_ms: 20_000,
            complete: true,
            ..Default::default()
        };
        assert!(!r.is_damaged());
        assert_eq!(r.label(), "clean");
    }

    #[test]
    fn labels_are_distinct_and_stable() {
        let damaged = IntegrityReport {
            read_errors: 1,
            complete: true,
            ..Default::default()
        };
        let unreadable = IntegrityReport {
            read_errors: MAX_READ_ERRORS,
            complete: false,
            ..Default::default()
        };
        let clean = IntegrityReport {
            complete: true,
            ..Default::default()
        };
        let short = IntegrityReport {
            declared_ms: Some(20_000),
            scanned_ms: 0,
            complete: true,
            ..Default::default()
        };
        let labels = [
            clean.label(),
            damaged.label(),
            unreadable.label(),
            short.label(),
        ];
        assert_eq!(
            labels,
            ["clean", "damaged", "unreadable", "short"],
            "metric labels are a dashboard contract"
        );
    }

    /// The failure shape libav reports as success: zero the packets of a 20 s
    /// file and it returns one packet then a clean end-of-file. No error, no
    /// flag — only the header's own claim shows the body is gone.
    #[test]
    fn a_body_that_stops_far_short_of_the_header_is_damage() {
        let r = IntegrityReport {
            packets: 1,
            declared_ms: Some(20_000),
            scanned_ms: 0,
            complete: true,
            ..Default::default()
        };
        assert!(r.is_short());
        assert!(r.is_damaged(), "no error was raised, but 20 s is missing");
        assert_eq!(r.shortfall_ms(), 20_000);
        assert!(r.summary().contains("20000 ms"), "{}", r.summary());
    }

    /// The margin exists so ordinary containers do not trip it: a file whose
    /// last packet lands a beat before the declared end is not damaged, and a
    /// container that declares no duration cannot be judged this way at all.
    #[test]
    fn an_ordinary_tail_gap_is_not_a_short_body() {
        let nearly = IntegrityReport {
            declared_ms: Some(2_400_000),
            scanned_ms: 2_399_000,
            complete: true,
            ..Default::default()
        };
        assert!(!nearly.is_short(), "1 s short of 40 min is not damage");

        let tiny = IntegrityReport {
            declared_ms: Some(4_000),
            scanned_ms: 0,
            complete: true,
            ..Default::default()
        };
        assert!(
            !tiny.is_short(),
            "below the absolute floor, a ratio alone must not condemn a clip"
        );

        let undeclared = IntegrityReport {
            declared_ms: None,
            scanned_ms: 0,
            complete: true,
            ..Default::default()
        };
        assert!(
            !undeclared.is_short(),
            "a container that promises nothing cannot break a promise"
        );
    }

    /// A demuxer that flags packets without ever failing a read is still
    /// reporting damage — counting only `read_errors` would miss it.
    #[test]
    fn a_corrupt_flagged_packet_alone_is_damage() {
        let r = IntegrityReport {
            packets: 500,
            corrupt_packets: 3,
            complete: true,
            ..Default::default()
        };
        assert!(r.is_damaged());
        assert_eq!(r.label(), "damaged");
    }

    #[test]
    fn the_summary_names_the_position_and_the_cause() {
        let r = IntegrityReport {
            packets: 70,
            read_errors: 1,
            first_fault_ms: Some(2_870),
            first_fault: Some("Invalid data found when processing input".into()),
            scanned_ms: 2_870,
            complete: true,
            ..Default::default()
        };
        let s = r.summary();
        assert!(s.contains("2870 ms"), "{s}");
        assert!(
            s.contains("Invalid data found when processing input"),
            "the demuxer's own words must survive to the log line: {s}"
        );
    }
}
