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

use std::collections::HashSet;
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

#[cfg(test)]
mod tests {
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
