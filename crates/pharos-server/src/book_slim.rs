//! Slimmed EPUB copies for e-readers with a small download and image budget
//! (B1451, ADR-0138).
//!
//! The Kobo refuses a download over 32 MB and keeps only 8 MB of pictures per
//! book, 2 MB each. A big EPUB is big because of its pictures, so the server
//! can hand such a client a copy whose pictures are scaled to the screen.
//!
//! This file starts with the signal: every slim decision lands on
//! `pharos_book_slim_total{outcome}`, so the verdict is queryable before any
//! reader depends on it.

/// What a request for a slimmed copy ended in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlimOutcome {
    /// A slimmed copy was built and cached, and is what is served.
    Slimmed,
    /// A previously built slimmed copy is served from the cache.
    CacheHit,
    /// An earlier decision that the original is the answer is served from the
    /// cache, without opening the book.
    CacheHitOriginal,
    /// No picture is larger than the box; the original is served.
    AlreadyFits,
    /// The re-packed copy was not smaller; the original is served.
    NotSmaller,
    /// No book cache directory is configured, so nothing can be stored; the
    /// original is served.
    NoCache,
    /// The book could not be read or re-packed; the original is served.
    Failed,
}

impl SlimOutcome {
    pub const ALL: [SlimOutcome; 7] = [
        SlimOutcome::Slimmed,
        SlimOutcome::CacheHit,
        SlimOutcome::CacheHitOriginal,
        SlimOutcome::AlreadyFits,
        SlimOutcome::NotSmaller,
        SlimOutcome::NoCache,
        SlimOutcome::Failed,
    ];

    /// The `outcome` label value. A dashboard contract: do not rename.
    pub fn label(self) -> &'static str {
        match self {
            SlimOutcome::Slimmed => "slimmed",
            SlimOutcome::CacheHit => "cache_hit",
            SlimOutcome::CacheHitOriginal => "cache_hit_original",
            SlimOutcome::AlreadyFits => "already_fits",
            SlimOutcome::NotSmaller => "not_smaller",
            SlimOutcome::NoCache => "no_cache",
            SlimOutcome::Failed => "failed",
        }
    }
}

/// Count one slim decision.
pub fn record_slim(outcome: SlimOutcome) {
    metrics::counter!("pharos_book_slim_total", "outcome" => outcome.label()).increment(1);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use std::collections::HashSet;

    #[test]
    fn every_outcome_has_its_own_label() {
        let labels: HashSet<_> = SlimOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), SlimOutcome::ALL.len());
    }

    /// Disarm check: make `record_slim` a no-op and the counter is absent.
    #[test]
    fn a_decision_is_counted_under_its_label() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            record_slim(SlimOutcome::Slimmed);
            record_slim(SlimOutcome::Slimmed);
            record_slim(SlimOutcome::CacheHit);
        });
        let count = |outcome: &str| -> u64 {
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .filter(|(k, ..)| {
                    k.key().name() == "pharos_book_slim_total"
                        && k.key()
                            .labels()
                            .any(|l| l.key() == "outcome" && l.value() == outcome)
                })
                .map(|(.., v)| match v {
                    DebugValue::Counter(n) => n,
                    _ => 0,
                })
                .sum()
        };
        assert_eq!(count("slimmed"), 2);
        assert_eq!(count("cache_hit"), 1);
        assert_eq!(count("failed"), 0);
    }
}
