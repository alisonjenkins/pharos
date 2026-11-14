//! How each line of a song was placed, and why a song kept today's answer
//! (spec 042, FR-008): the per-song summary behind the placement counters
//! and the song's log line.

use crate::obs::{LINE_PLACEMENT_TOTAL, SONG_FALLBACK_TOTAL};

/// How one lyric line got its word times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Inside a recogniser anchor, at the recogniser's word times.
    Anchored,
    /// Inside a recogniser anchor, refined letter by letter.
    Refined,
    /// Between two anchors, by today's per-line window clamped to the gap.
    Between,
    /// Today's per-line answer, unchanged.
    Kept,
}

impl Method {
    pub const ALL: [Self; 4] = [Self::Anchored, Self::Refined, Self::Between, Self::Kept];

    /// The `method` label of `pharos_aligner_line_placement_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anchored => "anchored",
            Self::Refined => "refined",
            Self::Between => "between",
            Self::Kept => "kept",
        }
    }
}

/// Why a whole song kept today's per-line answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The recogniser pipeline is not in this build or not switched on.
    Off,
    /// Too few lines were anchored to trust the recogniser.
    LowAnchorShare,
    /// Separating the vocals failed (out of memory, model error, timeout).
    SeparateFailed,
    /// Recognising the vocals failed.
    RecogniseFailed,
}

impl Fallback {
    pub const ALL: [Self; 4] = [
        Self::Off,
        Self::LowAnchorShare,
        Self::SeparateFailed,
        Self::RecogniseFailed,
    ];

    /// The `reason` label of `pharos_aligner_song_fallback_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::LowAnchorShare => "low_anchor_share",
            Self::SeparateFailed => "separate_failed",
            Self::RecogniseFailed => "recognise_failed",
        }
    }
}

/// One song's placement: lines per method, the share the recogniser
/// anchored, and why the song fell back, if it did.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    counts: [usize; 4],
    /// Anchored lines over lines with words; `None` when nothing was recognised.
    pub anchor_share: Option<f64>,
    /// Anchors set aside as a repeat heard in the wrong place.
    pub dropped_anchors: usize,
    pub fallback: Option<Fallback>,
}

impl Placement {
    /// Every one of `lines` (lines with words) kept today's answer, because of `why`.
    pub fn kept(lines: usize, why: Fallback) -> Self {
        let mut counts = [0; 4];
        counts[Method::Kept as usize] = lines;
        Self {
            counts,
            anchor_share: None,
            dropped_anchors: 0,
            fallback: Some(why),
        }
    }

    /// Lines per method, from each line's method.
    pub fn from_methods(methods: impl IntoIterator<Item = Method>, anchor_share: f64) -> Self {
        let mut counts = [0; 4];
        for m in methods {
            counts[m as usize] += 1;
        }
        Self {
            counts,
            anchor_share: Some(anchor_share),
            dropped_anchors: 0,
            fallback: None,
        }
    }

    pub fn count(&self, method: Method) -> usize {
        self.counts[method as usize]
    }

    /// Adds this song to the placement counters and, if it fell back, to the fallback counter.
    pub fn record(&self) {
        for m in Method::ALL {
            let n = self.count(m);
            if n > 0 {
                metrics::counter!(LINE_PLACEMENT_TOTAL, "method" => m.label()).increment(n as u64);
            }
        }
        if let Some(f) = self.fallback {
            metrics::counter!(SONG_FALLBACK_TOTAL, "reason" => f.label()).increment(1);
        }
    }

    /// The `fallback` log field: the reason, or `none`.
    pub fn fallback_label(&self) -> &'static str {
        self.fallback.map_or("none", Fallback::label)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::{Fallback, Method, Placement};
    use crate::obs::{LINE_PLACEMENT_TOTAL, SONG_FALLBACK_TOTAL};

    #[test]
    fn labels_are_distinct() {
        let mut m: Vec<_> = Method::ALL.iter().map(|m| m.label()).collect();
        m.sort_unstable();
        m.dedup();
        assert_eq!(m.len(), Method::ALL.len());
        let mut f: Vec<_> = Fallback::ALL.iter().map(|f| f.label()).collect();
        f.sort_unstable();
        f.dedup();
        assert_eq!(f.len(), Fallback::ALL.len());
    }

    fn counter<'a>(
        snapshot: &'a [(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        label: &str,
        value: &str,
    ) -> Option<&'a DebugValue> {
        snapshot
            .iter()
            .find(|(k, ..)| {
                k.key().name() == name
                    && k.key()
                        .labels()
                        .any(|l| l.key() == label && l.value() == value)
            })
            .map(|(.., v)| v)
    }

    #[test]
    fn a_kept_song_counts_its_lines_and_its_reason() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        Placement::kept(3, Fallback::Off).record();
        let snapshot = snapshotter.snapshot().into_vec();
        let c = |name, label, value| counter(&snapshot, name, label, value);
        assert_eq!(
            c(LINE_PLACEMENT_TOTAL, "method", "kept"),
            Some(&DebugValue::Counter(3))
        );
        assert_eq!(c(LINE_PLACEMENT_TOTAL, "method", "anchored"), None);
        assert_eq!(
            c(SONG_FALLBACK_TOTAL, "reason", "off"),
            Some(&DebugValue::Counter(1))
        );
    }

    #[test]
    fn a_placed_song_counts_each_method_and_no_fallback() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let p = Placement::from_methods(
            [
                Method::Refined,
                Method::Refined,
                Method::Between,
                Method::Kept,
            ],
            0.5,
        );
        assert_eq!(p.fallback_label(), "none");
        p.record();
        let snapshot = snapshotter.snapshot().into_vec();
        let c = |name, label, value| counter(&snapshot, name, label, value);
        assert_eq!(
            c(LINE_PLACEMENT_TOTAL, "method", "refined"),
            Some(&DebugValue::Counter(2))
        );
        assert_eq!(
            c(LINE_PLACEMENT_TOTAL, "method", "between"),
            Some(&DebugValue::Counter(1))
        );
        assert_eq!(
            c(LINE_PLACEMENT_TOTAL, "method", "kept"),
            Some(&DebugValue::Counter(1))
        );
        assert!(snapshot
            .iter()
            .all(|(k, ..)| k.key().name() != SONG_FALLBACK_TOTAL));
    }
}
