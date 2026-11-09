//! Spec 033 — telling apart same-named series. The signals the series matcher
//! records about which show a folder is.

use pharos_core::{MatchDecidedBy, MatchDecision};

/// What became of one show's match in a series pass, across every provider
/// asked. A metric label (`pharos_series_match_total{outcome}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeriesMatchOutcome {
    /// A candidate won and nothing rivalled it.
    Matched,
    /// A candidate won, but the runner-up was as good a reading
    /// ([`MatchDecision::ambiguous`]).
    Ambiguous,
    /// No provider had a candidate over the confidence floor.
    NoMatch,
    /// A match was chosen but its record did not arrive; the show is asked
    /// again next pass.
    Transient,
}

impl SeriesMatchOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Ambiguous => "ambiguous",
            Self::NoMatch => "no_match",
            Self::Transient => "transient",
        }
    }

    pub(crate) fn of(decision: &MatchDecision) -> Self {
        match (&decision.chosen, decision.ambiguous) {
            (None, _) => Self::NoMatch,
            (Some(_), true) => Self::Ambiguous,
            (Some(_), false) => Self::Matched,
        }
    }
}

/// `decided_by` label for a decision; `none` when nothing was chosen.
fn decided_by_label(decided_by: Option<MatchDecidedBy>) -> &'static str {
    decided_by.map_or("none", MatchDecidedBy::label)
}

/// Count one show's final match outcome, once per show per pass.
pub(crate) fn record_series_match(outcome: SeriesMatchOutcome, decided_by: Option<MatchDecidedBy>) {
    metrics::counter!(
        "pharos_series_match_total",
        "outcome" => outcome.label(),
        "decided_by" => decided_by_label(decided_by),
    )
    .increment(1);
}

/// The `series match` decision line: one per provider asked, matched or not,
/// with every candidate it weighed.
pub(crate) fn log_series_match(
    provider: &str,
    series_key: &str,
    query: &str,
    year: Option<u32>,
    decision: &MatchDecision,
) {
    let candidates: Vec<String> = decision
        .candidates
        .iter()
        .map(|c| {
            format!(
                "{}:{:?}:{}:{:.3}{}",
                c.id,
                c.title,
                c.year.map_or_else(|| "-".to_string(), |y| y.to_string()),
                c.score,
                if c.exact { ":exact" } else { "" }
            )
        })
        .collect();
    tracing::info!(
        provider,
        series_key,
        query,
        year,
        candidates = ?candidates,
        chosen = decision.chosen.as_ref().map(|c| c.id.as_str()),
        runner_up = decision.runner_up.as_ref().map(|c| c.id.as_str()),
        margin = decision.margin(),
        decided_by = decided_by_label(decision.decided_by),
        outcome = SeriesMatchOutcome::of(decision).label(),
        "series match"
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn labels_are_stable_and_distinct() {
        let outcomes = [
            SeriesMatchOutcome::Matched,
            SeriesMatchOutcome::Ambiguous,
            SeriesMatchOutcome::NoMatch,
            SeriesMatchOutcome::Transient,
        ];
        let set: BTreeSet<_> = outcomes.iter().map(|o| o.label()).collect();
        assert_eq!(set.len(), outcomes.len());
        assert_eq!(SeriesMatchOutcome::Ambiguous.label(), "ambiguous");
        assert_eq!(decided_by_label(None), "none");
    }
}
