//! Weak runs (B1402): consecutive lines the per-line pass matched badly,
//! between lines it matched well. Lyrics with mistimed stamps around an
//! instrumental break put each line's window in the wrong place, and the
//! model squeezes the words into whatever singing the window holds.

use std::ops::Range;

use crate::ctc::{align_words, Emission};
use crate::line::{frame_ms, LineSpans};
use crate::tokens::{tokenize, CLASSES, STAR_TOKEN};
use crate::AlignError;

/// A line scoring below this is weak; at or above it, an anchor. Well-timed
/// lines measured 0.13 to 0.55, mistimed ones 0.02 to 0.12.
pub const WEAK_SCORE: f64 = 0.14;

/// A re-search replaces pass 1 only when its mean score beats pass 1's by
/// this much (V1372): a long window can place mumbled words anywhere at a
/// score barely above pass 1's.
pub const ACCEPT_MARGIN: f64 = 0.02;

/// The longest window a run is re-searched in. Its CTC trellis grows with
/// frames x characters, and a longer run has more ways to go wrong.
pub const MAX_RUN_MS: i64 = 150_000;

/// Samples of one forward pass in a re-search: 30 s, about as long as the
/// widest offset probe, so a run's window costs no more memory than the
/// song's other windows.
pub const CHUNK_SAMPLES: usize = 30 * 16_000;

/// Audio either side of a chunk the model hears but whose frames are
/// dropped, so a word across a chunk boundary is heard whole: 1 s.
pub const CONTEXT_SAMPLES: usize = 16_000;

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

impl Open {
    /// Ends the run before line `next` (or the song's end), its audio
    /// stopping at `end_ms`.
    pub fn close(self, next: usize, end_ms: i64) -> Run {
        Run {
            lines: self.first..next,
            window_ms: (self.start_ms, end_ms),
            before: mean_score(&self.scores),
        }
    }
}

/// A closed weak run, ready to re-search.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Line indices, empty lines inside included.
    pub lines: Range<usize>,
    /// The audio to search, in song milliseconds.
    pub window_ms: (i64, i64),
    /// Pass 1's mean score over the run's worded lines, untimed ones as 0.
    pub before: f64,
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

/// V1372: a re-search replaces pass 1 only when it scores better by
/// [`ACCEPT_MARGIN`].
pub fn accept(before: f64, after: f64) -> bool {
    after >= before + ACCEPT_MARGIN
}

/// One forward pass of a long window: the samples it hears (`input`) and
/// those whose frames it keeps (`keep`). The keeps tile the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub input: Range<usize>,
    pub keep: Range<usize>,
}

/// `n` samples cut into [`CHUNK_SAMPLES`] keeps, each heard with up to
/// [`CONTEXT_SAMPLES`] either side.
pub fn chunks(n: usize) -> Vec<Chunk> {
    (0..n)
        .step_by(CHUNK_SAMPLES)
        .map(|start| {
            let end = (start + CHUNK_SAMPLES).min(n);
            Chunk {
                input: start.saturating_sub(CONTEXT_SAMPLES)..(end + CONTEXT_SAMPLES).min(n),
                keep: start..end,
            }
        })
        .collect()
}

/// The emission of `pcm`, run through `forward` a chunk at a time and the
/// kept frames joined; a frame belongs to the chunk whose keep holds its
/// centre. `None` as soon as `forward` gives none.
pub fn stitched<E>(
    pcm: &[f32],
    mut forward: impl FnMut(&[f32]) -> Result<Option<Vec<f32>>, E>,
) -> Result<Option<Vec<f32>>, E> {
    let mut out = Vec::new();
    for chunk in chunks(pcm.len()) {
        let Some(input) = pcm.get(chunk.input.clone()) else {
            continue;
        };
        let Some(rows) = forward(input)? else {
            return Ok(None);
        };
        let frames = rows.len() / CLASSES;
        let len = input.len();
        for (f, row) in rows.chunks_exact(CLASSES).enumerate() {
            let centre = chunk.input.start + (2 * f + 1) * len / (2 * frames);
            if chunk.keep.contains(&centre) {
                out.extend_from_slice(row);
            }
        }
    }
    Ok(Some(out))
}

/// Every line of a run aligned at once over one window's emission: a star
/// before the first line and after each, so the singing between lines and
/// around them is soaked up, and the lines stay in order. Times are
/// relative to the window; each line's score is the mean over its own
/// words' character spans, as [`crate::line::align_line`] scores one line,
/// the stars left out.
pub fn align_run<S: AsRef<str>>(
    emission: &Emission<'_>,
    n_samples: usize,
    lines: &[Vec<S>],
) -> Result<Vec<LineSpans>, AlignError> {
    let mut all: Vec<&str> = vec![STAR_TOKEN];
    let mut owner: Vec<Option<usize>> = vec![None];
    for (i, tokens) in lines.iter().enumerate() {
        for t in tokens {
            all.push(t.as_ref());
            owner.push(Some(i));
        }
        all.push(STAR_TOKEN);
        owner.push(None);
    }
    let spans = align_words(emission, &tokenize(&all)?)?;
    let frames = emission.frames();
    let mut out: Vec<(LineSpans, f64, usize)> = lines
        .iter()
        .map(|_| (LineSpans::default(), 0.0, 0))
        .collect();
    for (word, line) in spans.iter().zip(owner) {
        let Some((spans, sum, n)) = line.and_then(|i| out.get_mut(i)) else {
            continue;
        };
        spans.times.push(match (word.first(), word.last()) {
            (Some(first), Some(last)) => (
                frame_ms(n_samples, frames, first.start),
                frame_ms(n_samples, frames, last.end),
            ),
            _ => (0, 0),
        });
        *sum += word.iter().map(|c| f64::from(c.score)).sum::<f64>();
        *n += word.len();
    }
    Ok(out
        .into_iter()
        .map(|(mut spans, sum, n)| {
            spans.score = (n > 0).then(|| sum / n as f64);
            spans
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    use crate::tokens::label;

    #[test]
    fn a_re_search_must_beat_pass_one_by_the_margin() {
        assert!(accept(0.10, 0.10 + ACCEPT_MARGIN));
        assert!(!accept(0.10, 0.10 + ACCEPT_MARGIN - 0.001));
        assert!(!accept(0.10, 0.10), "no better is not enough");
        assert!(!accept(0.10, 0.05));
    }

    #[test]
    fn chunks_tile_the_window_and_hear_context_inside_it() {
        assert!(chunks(0).is_empty());
        let n = CHUNK_SAMPLES * 2 + CHUNK_SAMPLES / 2;
        let got = chunks(n);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].keep, 0..CHUNK_SAMPLES);
        assert_eq!(got[0].input, 0..CHUNK_SAMPLES + CONTEXT_SAMPLES);
        assert_eq!(
            got[1].input,
            CHUNK_SAMPLES - CONTEXT_SAMPLES..2 * CHUNK_SAMPLES + CONTEXT_SAMPLES
        );
        assert_eq!(got[2].keep, 2 * CHUNK_SAMPLES..n);
        assert_eq!(got[2].input, 2 * CHUNK_SAMPLES - CONTEXT_SAMPLES..n);
        for pair in got.windows(2) {
            assert_eq!(pair[0].keep.end, pair[1].keep.start);
        }
        assert_eq!(
            chunks(100),
            vec![Chunk {
                input: 0..100,
                keep: 0..100
            }]
        );
    }

    /// A fake model with a 320-sample frame whose every row holds its
    /// frame's index in the whole window: stitched, they count up once.
    #[test]
    fn stitched_frames_follow_on_without_gaps_or_repeats() {
        let n = CHUNK_SAMPLES * 2 + 320 * 40;
        let pcm: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let got = stitched(&pcm, |input: &[f32]| {
            let first = input[0] as usize / 320;
            let rows: Vec<f32> = (0..input.len() / 320)
                .flat_map(|f| [(first + f) as f32; CLASSES])
                .collect();
            Ok::<_, ()>(Some(rows))
        })
        .unwrap()
        .unwrap();
        let frames: Vec<usize> = got.chunks_exact(CLASSES).map(|r| r[0] as usize).collect();
        assert_eq!(frames, (0..n / 320).collect::<Vec<_>>());
        let none = stitched(&pcm, |_: &[f32]| Ok::<_, ()>(None)).unwrap();
        assert_eq!(none, None, "a model with no emission gives none");
    }

    /// Log-probabilities over `frames`: the blank everywhere but where
    /// `at` puts a character, at probability `p`.
    fn emission_with(frames: usize, at: &[(usize, char)], p: f32) -> Vec<f32> {
        let mut e = vec![(1e-4f32).ln(); frames * CLASSES];
        for f in 0..frames {
            e[f * CLASSES] = (0.99f32).ln();
            e[f * CLASSES + CLASSES - 1] = 0.0;
        }
        for &(f, c) in at {
            let l = label(c).unwrap() as usize;
            e[f * CLASSES] = (1e-4f32).ln();
            e[f * CLASSES + l] = p.ln();
        }
        e
    }

    /// A repeated line is placed twice, in order, where it is sung; each
    /// line scores its own characters only, not the stars around it.
    #[test]
    fn a_run_places_repeated_lines_in_order_and_scores_each_alone() {
        let frames = 60;
        let sung = [
            (10, 'l'),
            (11, 'a'),
            (30, 'l'),
            (31, 'a'),
            (50, 'n'),
            (51, 'o'),
        ];
        let e = emission_with(frames, &sung, 0.6);
        let em = Emission::new(&e, frames, CLASSES).unwrap();
        let lines = vec![vec!["la"], vec!["la"], vec!["no"]];
        let got = align_run(&em, 320 * frames, &lines).unwrap();
        let firsts: Vec<u64> = got.iter().map(|l| l.times[0].0).collect();
        assert_eq!(firsts, vec![200, 600, 1000], "20 ms frames");
        for line in &got {
            let s = line.score.unwrap();
            assert!((s - 0.6).abs() < 1e-3, "{s}: stars would lift it towards 1");
        }
    }

    #[test]
    fn a_run_too_long_for_its_window_is_an_error() {
        let e = emission_with(3, &[], 0.6);
        let em = Emission::new(&e, 3, CLASSES).unwrap();
        assert!(matches!(
            align_run(&em, 960, &[vec!["longer"]]),
            Err(AlignError::TooFewFrames { .. })
        ));
    }

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
