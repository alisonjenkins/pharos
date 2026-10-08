//! Test support: a [`DebuggingRecorder`] snapshotter that can be read repeatedly.
//!
//! `metrics-util` 0.20's `Snapshotter::snapshot()` drains what it reads
//! (counters and gauges are swapped to zero, histogram buckets are cleared),
//! so the second read in a test sees nothing and a "was never recorded"
//! assertion passes vacuously. [`CumulativeSnapshotter`] folds every drain
//! into a running total, so each `snapshot()` reports everything recorded
//! since the recorder was created.
//!
//! Gauges are the one approximation: a drain zeroes the gauge, so a later
//! zero reading cannot be told from an untouched gauge. A zero reading
//! therefore never overwrites a non-zero stored value. Assert gauges once,
//! after the code under test has settled.

use std::sync::{Arc, Mutex, PoisonError};

use metrics::{SharedString, Unit};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use metrics_util::CompositeKey;

type Row = (CompositeKey, Option<Unit>, Option<SharedString>, Total);
type DebugRow = (CompositeKey, Option<Unit>, Option<SharedString>, DebugValue);

#[derive(Debug)]
enum Total {
    Counter(u64),
    Gauge(f64),
    Histogram(Vec<f64>),
}

/// A [`Snapshotter`] whose reads accumulate instead of draining.
#[derive(Clone, Debug)]
pub struct CumulativeSnapshotter {
    inner: Snapshotter,
    totals: Arc<Mutex<Vec<Row>>>,
}

/// Cumulative view returned by [`CumulativeSnapshotter::snapshot`].
#[derive(Debug)]
pub struct CumulativeSnapshot(Vec<DebugRow>);

impl CumulativeSnapshot {
    /// Same shape as `metrics_util::debugging::Snapshot::into_vec`.
    pub fn into_vec(self) -> Vec<DebugRow> {
        self.0
    }
}

impl CumulativeSnapshotter {
    pub fn new(recorder: &DebuggingRecorder) -> Self {
        Self {
            inner: recorder.snapshotter(),
            totals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drain the recorder once, fold it into the running totals, and return
    /// the totals.
    pub fn snapshot(&self) -> CumulativeSnapshot {
        let drained = self.inner.snapshot().into_vec();
        let mut totals = self.totals.lock().unwrap_or_else(PoisonError::into_inner);
        for (key, unit, desc, value) in drained {
            match totals.iter_mut().find(|(k, ..)| *k == key) {
                Some((_, _, _, total)) => fold(total, value),
                None => totals.push((key, unit, desc, seed(value))),
            }
        }
        CumulativeSnapshot(
            totals
                .iter()
                .map(|(k, u, d, t)| (k.clone(), *u, d.clone(), render(t)))
                .collect(),
        )
    }
}

/// Shorthand for `CumulativeSnapshotter::new(recorder)`.
pub fn snapshotter(recorder: &DebuggingRecorder) -> CumulativeSnapshotter {
    CumulativeSnapshotter::new(recorder)
}

fn seed(value: DebugValue) -> Total {
    match value {
        DebugValue::Counter(n) => Total::Counter(n),
        DebugValue::Gauge(g) => Total::Gauge(g.into_inner()),
        DebugValue::Histogram(h) => Total::Histogram(h.into_iter().map(|v| v.0).collect()),
    }
}

fn fold(total: &mut Total, value: DebugValue) {
    match (total, value) {
        (Total::Counter(t), DebugValue::Counter(n)) => *t = t.saturating_add(n),
        (Total::Gauge(t), DebugValue::Gauge(g)) => {
            if g.into_inner() != 0.0 {
                *t = g.into_inner();
            }
        }
        (Total::Histogram(t), DebugValue::Histogram(h)) => t.extend(h.into_iter().map(|v| v.0)),
        (total, value) => *total = seed(value),
    }
}

fn render(total: &Total) -> DebugValue {
    match total {
        Total::Counter(n) => DebugValue::Counter(*n),
        Total::Gauge(g) => DebugValue::Gauge((*g).into()),
        Total::Histogram(h) => DebugValue::Histogram(h.iter().map(|v| (*v).into()).collect()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn counter(snap: CumulativeSnapshot, name: &str) -> Option<u64> {
        snap.into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| match v {
                DebugValue::Counter(n) if ck.key().name() == name => Some(n),
                _ => None,
            })
    }

    #[test]
    fn counter_survives_repeated_reads_and_keeps_accumulating() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("c").increment(2);
        });
        assert_eq!(counter(snapshotter.snapshot(), "c"), Some(2));
        assert_eq!(counter(snapshotter.snapshot(), "c"), Some(2));
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("c").increment(3);
        });
        assert_eq!(counter(snapshotter.snapshot(), "c"), Some(5));
    }

    #[test]
    fn unrecorded_metric_stays_absent() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("c").increment(1);
        });
        assert_eq!(counter(snapshotter.snapshot(), "other"), None);
    }

    #[test]
    fn histogram_accumulates_across_reads() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("h").record(1.0);
        });
        let _ = snapshotter.snapshot();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("h").record(2.0);
        });
        let got = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(_, _, _, v)| match v {
                DebugValue::Histogram(h) => Some(h.into_iter().map(|x| x.0).collect::<Vec<_>>()),
                _ => None,
            });
        assert_eq!(got, Some(vec![1.0, 2.0]));
    }

    #[test]
    fn gauge_keeps_its_value_across_reads() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("g").set(7.0);
        });
        for _ in 0..2 {
            let got =
                snapshotter
                    .snapshot()
                    .into_vec()
                    .into_iter()
                    .find_map(|(_, _, _, v)| match v {
                        DebugValue::Gauge(g) => Some(g.into_inner()),
                        _ => None,
                    });
            assert_eq!(got, Some(7.0));
        }
    }
}
