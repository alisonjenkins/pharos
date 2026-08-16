//! Where a play session's playhead actually landed — the FIRST segment index
//! each `PlaySessionId` requests, reported once per session.
//!
//! A client tells the server where it wants to start only by which segment it
//! asks for. Nothing else on the wire carries it: the segment URL has no
//! position, the playlist is identical for every viewer of a title, and
//! `/PlaybackInfo` is negotiated before the player picks a time. So when a
//! player lands somewhere absurd, the server sees a perfectly ordinary segment
//! request and says nothing.
//!
//! That is what made the 2026-08-15 stuck-joiner a reconstruction. A SyncPlay
//! member joined a film 54 minutes in and her player instead requested segment
//! 1171 of 1172 — the final 0.773 s of a 1 h 57 m title, which cannot be
//! produced (its video track ends before the container does) and 500s on every
//! retry. Seven play sessions each failed identically. The diagnosis came from
//! reading raw request paths out of Loki and noticing the index was the last
//! one in the grid; the server had recorded the failure of segment 1171 twelve
//! ways and never once recorded that a session had STARTED there.
//!
//! One line and one counter per session make that self-reporting: a player
//! that starts at the tail is not a segment failure, it is a player aimed off
//! the end of the media, and the two need to be distinguishable in a query.
//! Only the first segment of a session is recorded — every later index is
//! ordinary playback, and counting those would bury the signal under the very
//! traffic it has to be visible against.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Above this many tracked sessions, forget them all and start again. A play
/// session is only interesting here for its first segment, so a cleared entry
/// costs at most one duplicate line if that session is still running — far
/// cheaper than an LRU, and this map has no other consumer.
const MAX_TRACKED_SESSIONS: usize = 4096;

/// Where in the media a session's first segment request landed. Bounded label
/// set — asserted distinct in `start_positions_have_distinct_labels`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartPosition {
    /// Segment 0 — a normal start-from-the-beginning.
    Head,
    /// Anywhere in between: a resume, a seek, or a SyncPlay join.
    Mid,
    /// The LAST segment in the grid. Legitimate only for a player that was
    /// already at the end of the title; on the first request of a session it
    /// means the player was aimed at or past the end of the media.
    Tail,
}

impl StartPosition {
    pub fn label(self) -> &'static str {
        match self {
            StartPosition::Head => "head",
            StartPosition::Mid => "mid",
            StartPosition::Tail => "tail",
        }
    }

    /// Classify `seg` against a grid of `total_segs` segments.
    ///
    /// `total_segs == 0` cannot happen for a served segment (the bounds check
    /// rejects it first) but is treated as `Mid` rather than panicking: a
    /// diagnostic must never be the thing that takes playback down.
    pub fn classify(seg: u32, total_segs: u32) -> Self {
        if seg == 0 {
            StartPosition::Head
        } else if total_segs > 0 && seg == total_segs - 1 {
            StartPosition::Tail
        } else {
            StartPosition::Mid
        }
    }
}

/// Remembers which play sessions have already reported their first segment.
#[derive(Default)]
pub struct SessionStarts {
    seen: Mutex<HashSet<String>>,
}

impl SessionStarts {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when this is the first segment `psid` has asked for — i.e. the
    /// caller should report it. Every later call for the same session is
    /// `false`.
    ///
    /// A request with no `PlaySessionId` is never reported: there is no
    /// session to attribute a start to, and the legacy clients that omit it
    /// would otherwise report a "first" segment on every request.
    pub fn note_first(&self, psid: Option<&str>) -> bool {
        let Some(psid) = psid else {
            return false;
        };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= MAX_TRACKED_SESSIONS {
            seen.clear();
        }
        seen.insert(psid.to_string())
    }
}

/// Report where a play session started, when this is its first segment.
///
/// `surface` is the delivery surface (`main`, `h264cmaf`, `vp9`, a named
/// variant) — bounded, and carried because the surfaces have independent
/// segment paths and one can be broken while the others are fine.
pub fn note_session_start(
    starts: &SessionStarts,
    psid: Option<&str>,
    media_id: u64,
    seg: u32,
    total_segs: Option<u32>,
    start_secs: f64,
    surface: &'static str,
) {
    if !starts.note_first(psid) {
        return;
    }
    let position = StartPosition::classify(seg, total_segs.unwrap_or(0));
    metrics::counter!(
        "pharos_hls_session_start_total",
        "position" => position.label(),
        "surface" => surface,
    )
    .increment(1);
    tracing::info!(
        media.id = media_id,
        seg,
        total_segs,
        start_secs,
        surface,
        position = position.label(),
        "hls: play session first segment"
    );
}

/// How many times one play session may ask for the SAME segment before the
/// server says so.
///
/// A healthy client fetches an index once. Two is ordinary across a seek, three
/// happens. Tonight's wedge served segment 1188 **152 times** to one session and
/// segment 929 118 times, so this threshold is not a tuning question — anything
/// in this range is a player that cannot use what it is being given.
const REFETCH_THRESHOLD: u32 = 5;

/// Above this many tracked (session, segment) pairs, forget them all. Same
/// trade as [`MAX_TRACKED_SESSIONS`]: a cleared entry costs at most one delayed
/// warning, which is far cheaper than an LRU on the segment hot path.
const MAX_TRACKED_REFETCHES: usize = 16_384;

/// Counts repeat serves of one segment to one play session (T126).
///
/// The gap this fills: on 2026-08-16 a viewer stared at a frozen frame for
/// fifteen minutes while every server-side signal read GREEN — 2765 serves
/// across 71 unique indices, a 38.9x repeat factor, every one a 200 with a
/// sub-millisecond cache read. That is what a wedged player looks like from
/// the server, and it looks EXCELLENT: the cache hit ratio approaches 1 and the
/// transcode histogram sees almost nothing, precisely because the client keeps
/// asking for bytes that are already warm. Every existing metric is a rate or a
/// latency and both improve as the failure worsens.
///
/// Note the asymmetry with `http_client_aborted_total`, which catches the
/// client that GIVES UP. This catches the one that never does.
#[derive(Default)]
pub struct SegmentRefetches {
    counts: Mutex<HashMap<(String, u32), u32>>,
}

impl SegmentRefetches {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one serve of `seg` to `psid`. Returns `Some(count)` exactly once,
    /// on the serve that crosses [`REFETCH_THRESHOLD`], so a spinning client
    /// produces ONE line rather than one per repeat — the failure being
    /// reported is unbounded by nature and must not take the log with it.
    pub fn note(&self, psid: Option<&str>, seg: u32) -> Option<u32> {
        let psid = psid?;
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if counts.len() >= MAX_TRACKED_REFETCHES {
            counts.clear();
        }
        let n = counts.entry((psid.to_string(), seg)).or_insert(0);
        *n += 1;
        (*n == REFETCH_THRESHOLD).then_some(*n)
    }
}

/// Report a segment one session keeps re-fetching.
///
/// `surface` matches [`note_session_start`]'s: bounded, and carried because a
/// wedge on one delivery surface says nothing about the others.
pub fn note_segment_serve(
    refetches: &SegmentRefetches,
    psid: Option<&str>,
    media_id: u64,
    seg: u32,
    surface: &'static str,
) {
    let Some(count) = refetches.note(psid, seg) else {
        return;
    };
    metrics::counter!("pharos_segment_refetch_total", "surface" => surface).increment(1);
    tracing::warn!(
        media.id = media_id,
        seg,
        surface,
        count,
        play_session = psid.unwrap_or("?"),
        "a play session has re-requested one segment past the point a healthy \
         client would — it is being served bytes it cannot use"
    );
}

#[cfg(test)]
mod tests {

    /// T126 — a session that keeps asking for one segment must be reported.
    ///
    /// The failure this names served segment 1188 to one session 152 times
    /// while every rate and latency metric read green.
    #[test]
    fn a_repeatedly_refetched_segment_is_reported_once() {
        let r = SegmentRefetches::new();
        let psid = Some("sess-a");
        // Below the threshold: ordinary playback, including a seek or two.
        for _ in 1..REFETCH_THRESHOLD {
            assert_eq!(r.note(psid, 42), None, "a normal refetch must stay quiet");
        }
        assert_eq!(
            r.note(psid, 42),
            Some(REFETCH_THRESHOLD),
            "crossing the threshold must report"
        );
        // …and ONLY once: the failure is unbounded by nature and must not take
        // the log with it.
        for _ in 0..50 {
            assert_eq!(r.note(psid, 42), None, "reported once, not once per repeat");
        }
    }

    /// The count is per (session, segment): one session's spinning must not be
    /// attributed to another's, and a session walking forward normally must
    /// never trip it.
    #[test]
    fn refetch_counts_do_not_bleed_across_sessions_or_segments() {
        let r = SegmentRefetches::new();
        for _ in 0..REFETCH_THRESHOLD {
            let _ = r.note(Some("sess-a"), 1);
        }
        assert_eq!(
            r.note(Some("sess-b"), 1),
            None,
            "another session asking for the same segment is not a refetch"
        );
        for seg in 0..50 {
            assert_eq!(
                r.note(Some("sess-c"), seg),
                None,
                "ordinary forward playback must never trip the threshold"
            );
        }
    }

    /// A request with no session cannot be attributed, exactly as with
    /// `note_first` — and legacy clients that omit it must not be reported on
    /// every request.
    #[test]
    fn a_sessionless_request_is_never_counted() {
        let r = SegmentRefetches::new();
        for _ in 0..100 {
            assert_eq!(r.note(None, 7), None);
        }
    }
    use super::*;
    use metrics_util::debugging::DebuggingRecorder;

    #[test]
    fn start_positions_have_distinct_labels() {
        const ALL: [StartPosition; 3] =
            [StartPosition::Head, StartPosition::Mid, StartPosition::Tail];
        assert_eq!(StartPosition::Head.label(), "head");
        assert_eq!(StartPosition::Mid.label(), "mid");
        assert_eq!(StartPosition::Tail.label(), "tail");
        let labels: HashSet<&str> = ALL.iter().map(|p| p.label()).collect();
        assert_eq!(
            labels.len(),
            ALL.len(),
            "start positions collide: {labels:?}"
        );
    }

    /// The classification that matters: 1171 of 1172 is the tail, and the
    /// segment before it is not. A player aimed off the end of the media has
    /// to be distinguishable from one that merely resumed late.
    #[test]
    fn the_last_segment_of_the_grid_is_the_tail() {
        assert_eq!(StartPosition::classify(1171, 1172), StartPosition::Tail);
        assert_eq!(StartPosition::classify(1170, 1172), StartPosition::Mid);
        assert_eq!(StartPosition::classify(0, 1172), StartPosition::Head);
        // Single-segment media: segment 0 is both ends. Head wins — a start
        // at the beginning is not evidence of anything wrong.
        assert_eq!(StartPosition::classify(0, 1), StartPosition::Head);
        // Unknown grid length must not manufacture a tail.
        assert_eq!(StartPosition::classify(1171, 0), StartPosition::Mid);
    }

    #[test]
    fn only_the_first_segment_of_a_session_is_reported() {
        let starts = SessionStarts::new();
        assert!(starts.note_first(Some("psid-a")));
        assert!(!starts.note_first(Some("psid-a")));
        assert!(starts.note_first(Some("psid-b")));
        // No session id → nothing to attribute a start to, ever.
        assert!(!starts.note_first(None));
        assert!(!starts.note_first(None));
    }

    /// A session whose FIRST request is the final segment must count as a
    /// tail start. This is the whole signal: without it, seven play sessions
    /// aimed off the end of a film were indistinguishable from seven
    /// ordinary sessions that happened to hit a broken segment.
    #[test]
    fn a_session_starting_on_the_final_segment_counts_as_tail() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let starts = SessionStarts::new();
        note_session_start(
            &starts,
            Some("psid-tail"),
            479_628_986_076_528_820,
            1171,
            Some(1172),
            7025.998,
            "h264cmaf",
        );
        // Same session, later segments — must not be counted again.
        note_session_start(
            &starts,
            Some("psid-tail"),
            479_628_986_076_528_820,
            1172,
            Some(1172),
            7031.998,
            "h264cmaf",
        );

        let snap = snapshotter.snapshot().into_vec();
        let starts_counted: Vec<Vec<String>> = snap
            .iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_hls_session_start_total")
            .map(|(ck, _, _, _)| {
                ck.key()
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect()
            })
            .collect();
        assert_eq!(
            starts_counted.len(),
            1,
            "exactly one start per session; got {starts_counted:?}"
        );
        assert!(
            starts_counted[0].contains(&"position=tail".to_string()),
            "a session starting on the final segment is a tail start; got {starts_counted:?}"
        );
    }
}
