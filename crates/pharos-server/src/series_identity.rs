//! Spec 033 — telling apart same-named series. The signals the series matcher
//! and the episode backfill record about which show a folder is and which
//! show each episode's provider record belongs to.

use pharos_core::{MatchDecidedBy, MatchDecision, SeriesMetadata};

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
    /// (B931: Cosmos 1980 files with 2014 NFO ids).
    NfoOtherSeries,
    /// The per-episode series search landed in another series than the
    /// folder's match (B933).
    SearchOtherSeries,
}

impl EpisodeRejectReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NfoOtherSeries => "nfo_other_series",
            Self::SearchOtherSeries => "search_other_series",
        }
    }
}

/// Whether an episode resolved on `provider` (by `source`, `"nfo_id"` or
/// `"search"`) to a record of show `record_series` disagrees with its
/// folder's pinned match. Only a comparison within one provider means
/// anything: a show pinned on TMDB says nothing about a TVDB series id.
pub(crate) fn episode_series_conflict(
    provider: &str,
    source: &str,
    record_series: Option<&str>,
    pinned: Option<&SeriesMetadata>,
) -> Option<EpisodeRejectReason> {
    let pinned = pinned?;
    if pinned.match_provider.as_deref() != Some(provider)
        || !matches!(pinned.match_source.as_deref(), Some("search" | "manual"))
    {
        return None;
    }
    let pinned_id = pinned.match_external_id.as_deref()?;
    let record_series = record_series?;
    if record_series == pinned_id {
        return None;
    }
    match source {
        "nfo_id" => Some(EpisodeRejectReason::NfoOtherSeries),
        "search" => Some(EpisodeRejectReason::SearchOtherSeries),
        _ => None,
    }
}

pub(crate) fn record_episode_rejected(reason: EpisodeRejectReason) {
    metrics::counter!(
        "pharos_episode_metadata_rejected_total",
        "reason" => reason.label(),
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
            EpisodeRejectReason::SearchOtherSeries,
        ];
        let set: BTreeSet<_> = reasons.iter().map(|r| r.label()).collect();
        assert_eq!(set.len(), reasons.len());
        assert_eq!(
            EpisodeRejectReason::NfoOtherSeries.label(),
            "nfo_other_series"
        );
        assert_eq!(decided_by_label(None), "none");
    }

    fn pinned(provider: &str, source: &str, id: &str) -> SeriesMetadata {
        SeriesMetadata {
            series_key: "/tv/Cosmos".into(),
            series_name: "Cosmos".into(),
            match_provider: Some(provider.into()),
            match_source: Some(source.into()),
            match_external_id: Some(id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn an_episode_conflicts_only_with_its_own_providers_pinned_show() {
        let cosmos_1980 = pinned("tvdb", "search", "74995");
        // Cosmos: the episode NFO's id reads through to the 2014 series.
        assert_eq!(
            episode_series_conflict("tvdb", "nfo_id", Some("260586"), Some(&cosmos_1980)),
            Some(EpisodeRejectReason::NfoOtherSeries)
        );
        assert_eq!(
            episode_series_conflict("tvdb", "search", Some("260586"), Some(&cosmos_1980)),
            Some(EpisodeRejectReason::SearchOtherSeries)
        );
        // The same show is no conflict.
        assert_eq!(
            episode_series_conflict("tvdb", "nfo_id", Some("74995"), Some(&cosmos_1980)),
            None
        );
        // Another provider's id says nothing.
        assert_eq!(
            episode_series_conflict("tmdb", "nfo_id", Some("1"), Some(&cosmos_1980)),
            None
        );
        // A show matched to nothing, or not yet matched, pins nothing.
        let none = pinned("tvdb", "none", "");
        assert_eq!(
            episode_series_conflict("tvdb", "nfo_id", Some("1"), Some(&none)),
            None
        );
        assert_eq!(
            episode_series_conflict("tvdb", "nfo_id", Some("1"), None),
            None
        );
        // A record that names no series cannot be compared.
        assert_eq!(
            episode_series_conflict("tvdb", "nfo_id", None, Some(&cosmos_1980)),
            None
        );
    }
}
