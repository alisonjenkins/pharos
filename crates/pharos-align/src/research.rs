//! Weak runs (B1402): consecutive lines the per-line pass matched badly,
//! between lines it matched well. Lyrics with mistimed stamps around an
//! instrumental break put each line's window in the wrong place, and the
//! model squeezes the words into whatever singing the window holds.

use std::ops::Range;

/// A line scoring below this is weak; at or above it, an anchor. Well-timed
/// lines measured 0.13 to 0.55, mistimed ones 0.02 to 0.12.
pub const WEAK_SCORE: f64 = 0.14;

/// Weak-run counts by how each ended, labelled `outcome`.
pub const WEAK_RUNS_TOTAL: &str = "pharos_aligner_weak_runs_total";

/// What became of one weak run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The re-search scored better by the margin and replaced pass 1.
    Accepted,
    /// The re-search did not beat pass 1; pass 1 stays.
    Rejected,
    /// The re-search failed (model or CTC error); pass 1 stays.
    Failed,
    /// No re-search was tried; pass 1 stays.
    Skipped,
}

impl Outcome {
    pub const ALL: [Self; 4] = [Self::Accepted, Self::Rejected, Self::Failed, Self::Skipped];

    /// The metric label: a dashboard contract, never renamed.
    pub fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Runs per outcome over one song, for its summary log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunCounts {
    pub accepted: usize,
    pub rejected: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl RunCounts {
    pub fn total(&self) -> usize {
        self.accepted + self.rejected + self.failed + self.skipped
    }

    fn add(&mut self, outcome: Outcome) {
        let slot = match outcome {
            Outcome::Accepted => &mut self.accepted,
            Outcome::Rejected => &mut self.rejected,
            Outcome::Failed => &mut self.failed,
            Outcome::Skipped => &mut self.skipped,
        };
        *slot += 1;
    }
}

/// One weak run and its verdict, as logged.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Line indices, first to last inclusive (empty lines inside included).
    pub lines: Range<usize>,
    /// The audio searched, in song milliseconds.
    pub window_ms: (i64, i64),
    /// Pass 1's mean score over the run's worded lines, untimed ones as 0.
    pub before: f64,
    /// The re-search's mean over the same lines, when it ran.
    pub after: Option<f64>,
    pub outcome: Outcome,
    /// Why, naming the value that decided it.
    pub reason: String,
}

/// Counts `report` into `counts` and the metric, and logs it.
pub fn record(rel_path: &str, report: &RunReport, counts: &mut RunCounts) {
    counts.add(report.outcome);
    metrics::counter!(WEAK_RUNS_TOTAL, "outcome" => report.outcome.label()).increment(1);
    tracing::info!(
        path = %rel_path,
        first_line = report.lines.start,
        last_line = report.lines.end.saturating_sub(1),
        window_start_ms = report.window_ms.0,
        window_end_ms = report.window_ms.1,
        before = format!("{:.3}", report.before),
        after = report.after.map(|a| format!("{a:.3}")).unwrap_or_default(),
        outcome = report.outcome.label(),
        reason = %report.reason,
        "weak run"
    );
}

/// A weak run still open: its first line, where its audio starts (the last
/// anchor's last word's end, or 0), and pass 1's score per worded line.
#[derive(Debug, Clone, PartialEq)]
pub struct Open {
    pub first: usize,
    pub start_ms: i64,
    pub scores: Vec<Option<f64>>,
}

/// A worded line is weak when it went untimed or scored below
/// [`WEAK_SCORE`]; a timed line with no score (a model that gives none)
/// is an anchor, so such a model never re-searches.
pub fn is_weak(timed: bool, score: Option<f64>) -> bool {
    !timed || score.is_some_and(|s| s < WEAK_SCORE)
}

/// The mean of `scores`, `None` counted as 0; 0 for none at all.
pub fn mean_score(scores: &[Option<f64>]) -> f64 {
    if scores.is_empty() {
        return 0.0;
    }
    scores.iter().map(|s| s.unwrap_or(0.0)).sum::<f64>() / scores.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let mut labels: Vec<&str> = Outcome::ALL.iter().map(|o| o.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Outcome::ALL.len());
    }

    #[test]
    fn untimed_or_low_scoring_lines_are_weak_and_unscored_ones_anchor() {
        assert!(is_weak(false, None));
        assert!(is_weak(false, Some(0.9)));
        assert!(is_weak(true, Some(WEAK_SCORE - 0.001)));
        assert!(!is_weak(true, Some(WEAK_SCORE)));
        assert!(!is_weak(true, None));
    }

    #[test]
    fn the_mean_counts_missing_scores_as_zero() {
        assert_eq!(mean_score(&[]), 0.0);
        assert!((mean_score(&[Some(0.2), None, Some(0.1)]) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn each_report_is_counted_and_metered_under_its_outcome() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let mut counts = RunCounts::default();
        for outcome in [Outcome::Accepted, Outcome::Rejected, Outcome::Rejected] {
            let report = RunReport {
                lines: 3..6,
                window_ms: (1_000, 9_000),
                before: 0.05,
                after: Some(0.1),
                outcome,
                reason: "test".into(),
            };
            record("a.mp3", &report, &mut counts);
        }
        assert_eq!(
            counts,
            RunCounts {
                accepted: 1,
                rejected: 2,
                ..RunCounts::default()
            }
        );
        let snapshot = snapshotter.snapshot().into_vec();
        let count = |label: &str| {
            snapshot
                .iter()
                .find(|(k, ..)| {
                    k.key().name() == WEAK_RUNS_TOTAL
                        && k.key()
                            .labels()
                            .any(|l| l.key() == "outcome" && l.value() == label)
                })
                .map(|(.., v)| match v {
                    DebugValue::Counter(c) => *c,
                    _ => 0,
                })
        };
        assert_eq!((count("accepted"), count("rejected")), (Some(1), Some(2)));
    }
}
