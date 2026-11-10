//! Spec 033 — telling apart same-named series. The signals the series matcher
//! and the episode backfill record about which show a folder is and which
//! show each episode's provider record belongs to.

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

/// Why an episode's provider record is not its show's. A metric label
/// (`pharos_episode_metadata_rejected_total{reason}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpisodeRejectReason {
    /// The episode NFO's id names another series than the folder's match
    /// (B931: older-show files with newer-show NFO ids); the record is set aside.
    NfoOtherSeries,
    /// The file's episode title is neither the pinned show's record at its
    /// number nor a neighbour's. Observation only.
    TitleMismatch,
}

impl EpisodeRejectReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NfoOtherSeries => "nfo_other_series",
            Self::TitleMismatch => "title_mismatch",
        }
    }
}

pub(crate) fn record_episode_rejected(reason: EpisodeRejectReason) {
    metrics::counter!(
        "pharos_episode_metadata_rejected_total",
        "reason" => reason.label(),
    )
    .increment(1);
}

/// A step a show's re-match took beyond a plain search (B990). A metric
/// label (`pharos_series_identity_repair_rematch_total{step}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RematchStep {
    /// A search narrowed by the year found nothing and was asked again
    /// without it.
    YearRetry,
    /// The search used the year the show's files state, which the stored
    /// column did not hold.
    StatedYear,
    /// The stored match's run was too short for the show's seasons, so the
    /// re-match left it out.
    PinExcluded,
    /// Every candidate left was too short, so the show kept its match,
    /// flagged.
    FlaggedTooShort,
}

impl RematchStep {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::YearRetry => "year_retry",
            Self::StatedYear => "stated_year",
            Self::PinExcluded => "pin_excluded",
            Self::FlaggedTooShort => "flagged_too_short",
        }
    }
}

pub(crate) fn record_rematch(step: RematchStep) {
    metrics::counter!(
        "pharos_series_identity_repair_rematch_total",
        "step" => step.label(),
    )
    .increment(1);
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
        let reasons = [
            EpisodeRejectReason::NfoOtherSeries,
            EpisodeRejectReason::TitleMismatch,
        ];
        let set: BTreeSet<_> = reasons.iter().map(|r| r.label()).collect();
        assert_eq!(set.len(), reasons.len());
        assert_eq!(
            EpisodeRejectReason::NfoOtherSeries.label(),
            "nfo_other_series"
        );
        assert_eq!(decided_by_label(None), "none");
        let steps = [
            RematchStep::YearRetry,
            RematchStep::StatedYear,
            RematchStep::PinExcluded,
            RematchStep::FlaggedTooShort,
        ];
        let set: BTreeSet<_> = steps.iter().map(|s| s.label()).collect();
        assert_eq!(set.len(), steps.len());
        assert_eq!(RematchStep::YearRetry.label(), "year_retry");
    }
}
