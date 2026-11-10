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
    /// Where in the media the damage is, merged into spans a player can mark
    /// on its seek bar. `None` on a verdict recorded before ranges existed; a
    /// damaged one of those is read again so it gains them.
    #[serde(default)]
    pub ranges: Option<Vec<DamageRange>>,
    /// Whether the container carries an index a seek can use (B1162, V1122).
    /// `Some(false)` is an AVI with neither an `idx1` chunk nor an OpenDML
    /// `indx`: every seek into it makes libav read the file from byte 0,
    /// so a seek near the end of a long file can wait over a minute for its first
    /// frame. `None` means not checked, either because the container keeps
    /// its index somewhere libav reads lazily (Matroska cues, MP4 `moov`), or
    /// because the verdict was recorded before this check existed.
    ///
    /// Always written: the worker sends this type as bincode, which has no
    /// field names, so a field left out would shift every one after it.
    #[serde(default)]
    pub seek_index: Option<bool>,
}

/// One span of damaged media, in ms. A fault the demuxer could place no more
/// precisely than one packet gives `start_ms == end_ms`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Faults closer together than this are one span: a viewer sees one broken
/// stretch, not forty marks a frame apart.
pub const RANGE_MERGE_GAP_MS: u64 = 2_000;

/// Spans kept per file. A file with more is damaged throughout, and the last
/// span is stretched to cover the rest rather than growing the memo.
pub const MAX_RANGES: usize = 64;

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
    /// Note a fault at `at_ms`, joining it to the last span when it is near.
    pub fn note_range(&mut self, at_ms: u64) {
        let ranges = self.ranges.get_or_insert_with(Vec::new);
        let full = ranges.len() >= MAX_RANGES;
        match ranges.last_mut() {
            Some(last)
                if full
                    || (at_ms.saturating_add(RANGE_MERGE_GAP_MS) >= last.start_ms
                        && at_ms <= last.end_ms.saturating_add(RANGE_MERGE_GAP_MS)) =>
            {
                last.start_ms = last.start_ms.min(at_ms);
                last.end_ms = last.end_ms.max(at_ms);
            }
            _ => ranges.push(DamageRange {
                start_ms: at_ms,
                end_ms: at_ms,
            }),
        }
    }

    /// Stretch the last span to `to_ms`: the first clean packet after a fault
    /// is where the damage visibly ends.
    pub fn close_range(&mut self, to_ms: u64) {
        if let Some(last) = self.ranges.as_mut().and_then(|r| r.last_mut()) {
            last.end_ms = last.end_ms.max(to_ms);
        }
    }

    /// The spans to show for this verdict, in media order. A verdict recorded before spans
    /// existed falls back to its first fault, or to the missing tail of a
    /// short body, so an old damaged entry still marks something.
    pub fn damage_ranges(&self) -> Vec<DamageRange> {
        if !self.is_damaged() {
            return Vec::new();
        }
        if let Some(r) = self.ranges.as_ref().filter(|r| !r.is_empty()) {
            let mut r = r.clone();
            r.sort_unstable_by_key(|d| (d.start_ms, d.end_ms));
            return r;
        }
        let mut out = Vec::new();
        let tail = self.is_short() || !self.complete;
        if let Some(at) = self
            .first_fault_ms
            .filter(|at| !tail || *at < self.scanned_ms)
        {
            out.push(DamageRange {
                start_ms: at,
                end_ms: at,
            });
        }
        if tail {
            out.push(DamageRange {
                start_ms: self.scanned_ms,
                end_ms: self
                    .declared_ms
                    .unwrap_or(self.scanned_ms)
                    .max(self.scanned_ms),
            });
        }
        out
    }

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

    /// Whether a seek into this file has to read it from the start.
    pub fn is_unindexed(&self) -> bool {
        self.seek_index == Some(false)
    }

    /// Whether anything was wrong with the container.
    pub fn is_damaged(&self) -> bool {
        self.corrupt_packets > 0
            || self.read_errors > 0
            || self.demux_errors > 0
            || self.is_short()
            || self.is_unindexed()
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
        } else if self.is_short() {
            "short"
        } else {
            "unindexed"
        }
    }

    /// One line for the scan log, naming the position and the cause.
    pub fn summary(&self) -> String {
        if !self.is_damaged() {
            return format!("clean ({} packets, {} ms)", self.packets, self.scanned_ms);
        }
        if self.label() == "unindexed" {
            return format!(
                "unindexed: no seek index, so every seek reads the file from the start; \
                 remux it (e.g. `ffmpeg -i in.avi -c copy out.mkv`) to fix ({} packets, {} ms)",
                self.packets, self.scanned_ms,
            );
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
        let unindexed = IntegrityReport {
            complete: true,
            seek_index: Some(false),
            ..Default::default()
        };
        let labels = [
            clean.label(),
            damaged.label(),
            unreadable.label(),
            short.label(),
            unindexed.label(),
        ];
        assert_eq!(
            labels,
            ["clean", "damaged", "unreadable", "short", "unindexed"],
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

    #[test]
    fn faults_close_together_are_one_span_and_far_apart_are_two() {
        let mut r = IntegrityReport::default();
        r.note_range(10_000);
        r.note_range(11_000);
        r.close_range(11_500);
        r.note_range(60_000);
        r.note_range(0);
        assert_eq!(
            r.ranges.unwrap(),
            vec![
                DamageRange {
                    start_ms: 10_000,
                    end_ms: 11_500
                },
                DamageRange {
                    start_ms: 60_000,
                    end_ms: 60_000
                },
                DamageRange {
                    start_ms: 0,
                    end_ms: 0
                },
            ],
            "a fault before the last span starts a span of its own"
        );
    }

    #[test]
    fn spans_are_shown_in_media_order() {
        let mut r = IntegrityReport {
            read_errors: 2,
            complete: true,
            ..Default::default()
        };
        r.note_range(60_000);
        r.note_range(0);
        let starts: Vec<u64> = r.damage_ranges().iter().map(|d| d.start_ms).collect();
        assert_eq!(starts, vec![0, 60_000]);
    }

    #[test]
    fn spans_stop_growing_at_the_cap_and_the_last_one_stretches() {
        let mut r = IntegrityReport::default();
        for i in 0..(MAX_RANGES as u64 + 10) {
            r.note_range(i * 10_000);
        }
        let ranges = r.ranges.unwrap();
        assert_eq!(ranges.len(), MAX_RANGES);
        assert_eq!(
            ranges.last().unwrap().end_ms,
            (MAX_RANGES as u64 + 9) * 10_000
        );
    }

    #[test]
    fn an_old_verdict_without_spans_still_marks_its_first_fault() {
        let r = IntegrityReport {
            read_errors: 1,
            first_fault_ms: Some(2_870),
            complete: true,
            ..Default::default()
        };
        assert_eq!(
            r.damage_ranges(),
            vec![DamageRange {
                start_ms: 2_870,
                end_ms: 2_870
            }]
        );
        let short = IntegrityReport {
            declared_ms: Some(20_000),
            scanned_ms: 1_000,
            complete: true,
            ..Default::default()
        };
        assert_eq!(
            short.damage_ranges(),
            vec![DamageRange {
                start_ms: 1_000,
                end_ms: 20_000
            }]
        );
        let abandoned = IntegrityReport {
            read_errors: 40,
            first_fault_ms: Some(2_870),
            scanned_ms: 50_000,
            declared_ms: Some(600_000),
            complete: false,
            ..Default::default()
        };
        assert_eq!(
            abandoned.damage_ranges(),
            vec![
                DamageRange {
                    start_ms: 2_870,
                    end_ms: 2_870
                },
                DamageRange {
                    start_ms: 50_000,
                    end_ms: 600_000
                },
            ],
            "both the first fault and the unread tail are marked"
        );
        assert!(IntegrityReport::default().damage_ranges().is_empty());
    }

    /// B1162 / V1122: an AVI with no index is reported, and says what to do.
    /// Seeking into it reads the whole file up to the seek point, which is a
    /// fault a viewer meets as a minute-long seek, so it is listed with the
    /// damaged files rather than passing as clean.
    #[test]
    fn a_container_without_a_seek_index_is_reported_with_its_remedy() {
        let r = IntegrityReport {
            packets: 300_000,
            scanned_ms: 5_880_000,
            declared_ms: Some(5_880_000),
            complete: true,
            ranges: Some(Vec::new()),
            seek_index: Some(false),
            ..Default::default()
        };
        assert!(r.is_unindexed());
        assert!(
            r.is_damaged(),
            "a minute-long seek is a fault a viewer meets"
        );
        assert_eq!(r.label(), "unindexed");
        assert!(r.summary().contains("no seek index"), "{}", r.summary());
        assert!(r.summary().contains("-c copy"), "{}", r.summary());
        assert!(
            r.damage_ranges().is_empty(),
            "no span of the media is broken, so nothing is marked on the seek bar"
        );

        let indexed = IntegrityReport {
            seek_index: Some(true),
            ..r.clone()
        };
        assert!(!indexed.is_damaged());
        let unchecked = IntegrityReport {
            seek_index: None,
            ..r
        };
        assert!(!unchecked.is_damaged(), "not checked is not a fault");
    }

    /// A verdict written to the memo before the check existed reads back as
    /// unchecked.
    #[test]
    fn a_verdict_from_before_the_check_reads_as_unchecked() {
        let old = r#"{"packets":3,"corrupt_packets":0,"read_errors":0,"demux_errors":0,"first_fault_ms":null,"first_fault":null,"scanned_ms":4,"declared_ms":null,"complete":true}"#;
        let r: IntegrityReport = serde_json::from_str(old).unwrap();
        assert_eq!(r.seek_index, None);
    }
}
