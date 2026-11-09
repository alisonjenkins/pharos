//! Spec 033 (V891) — telling apart same-named shows by their episodes.
//!
//! When two or more search candidates match a show's name exactly and no
//! folder or file year separates them, the provider's order is no reason to
//! prefer one (B930: it handed every bare "Battlestar Galactica" folder to the
//! 1978 show). Instead each candidate's episode list is scored against the
//! local files, and `tvshow.nfo` adds one vote.

use pharos_core::{title_similarity, MatchDecidedBy};
use pharos_scanner::metadata::nfo::ShowNfoEvidence;

use crate::online_enrich::EpisodeListing;

/// Most exact-name candidates whose episode lists are fetched: each costs one
/// request (TVDB) or one per local season (TMDB), and the deployed library's
/// worst case, Battlestar Galactica, has three.
pub(crate) const MAX_SCORED: usize = 3;

/// A winner leading the runner-up by less than this is a close call, flagged
/// for an admin. Scores run 0 to 1 (plus [`NFO_VOTE`]); 0.2 is a fifth of the
/// local episodes or titles fitting one show and not the other.
pub(crate) const CLEAR_MARGIN: f32 = 0.2;

/// The `tvshow.nfo` vote. Small enough that it only decides between shows the
/// episodes fit about equally, since the deployed NFOs are wrong as often as
/// right (V890).
pub(crate) const NFO_VOTE: f32 = 0.1;

/// Most filename episode titles compared per show.
pub(crate) const MAX_TITLES: usize = 10;

/// Lowest [`title_similarity`] at which a filename title names the listed
/// episode: the threshold B558 and the episode title check use.
const SAME_TITLE: f32 = 0.8;

/// One local episode file of the show being matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalEpisode {
    pub season: u32,
    pub episode: u32,
    /// The episode's name from its filename, if it states one.
    pub title: Option<String>,
}

/// How well `listing` fits `local`, 0 to 1: the share of local (season,
/// episode) pairs the listing has, averaged with the share of filename titles
/// that match the listed title at the same number when any file states one.
pub(crate) fn fit(local: &[LocalEpisode], listing: &[EpisodeListing]) -> f32 {
    if local.is_empty() {
        return 0.0;
    }
    let at = |s: u32, e: u32| listing.iter().find(|l| l.season == s && l.episode == e);
    let covered = local
        .iter()
        .filter(|l| at(l.season, l.episode).is_some())
        .count();
    let coverage = ratio(covered, local.len());
    let titled: Vec<(&LocalEpisode, &str)> = local
        .iter()
        .filter_map(|l| Some((l, l.title.as_deref()?)))
        .take(MAX_TITLES)
        .collect();
    if titled.is_empty() {
        return coverage;
    }
    let same = titled
        .iter()
        .filter(|(l, title)| {
            at(l.season, l.episode)
                .and_then(|listed| listed.title.as_deref())
                .is_some_and(|listed| title_similarity(title, listed) >= SAME_TITLE)
        })
        .count();
    0.5 * coverage + 0.5 * ratio(same, titled.len())
}

#[allow(clippy::cast_precision_loss)]
fn ratio(n: usize, of: usize) -> f32 {
    n as f32 / of as f32
}

/// One scored candidate, by provider id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub id: String,
    pub year: Option<u32>,
}

/// Which candidate, if any, `nfo` votes for: the one its id for `provider`
/// names, else the only one premiering in its year. An NFO year is a vote,
/// never a filter (V890).
pub(crate) fn nfo_vote(
    provider: &str,
    candidates: &[Candidate],
    nfo: Option<&ShowNfoEvidence>,
) -> Option<usize> {
    let nfo = nfo?;
    let id = match provider {
        "tvdb" => nfo.tvdb.as_deref(),
        "tmdb" => nfo.tmdb.as_deref(),
        _ => None,
    };
    if let Some(at) = id.and_then(|id| candidates.iter().position(|c| c.id == id)) {
        return Some(at);
    }
    let year = nfo.year?;
    let mut same_year = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.year == Some(year));
    match (same_year.next(), same_year.next()) {
        (Some((at, _)), None) => Some(at),
        _ => None,
    }
}

/// The episode-fit decision among scored candidates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verdict {
    pub chosen: usize,
    pub runner_up: Option<usize>,
    /// Each candidate's fit plus its vote, in candidate order.
    pub scores: Vec<f32>,
    pub decided_by: MatchDecidedBy,
    /// The winner leads by less than [`CLEAR_MARGIN`].
    pub close: bool,
}

impl Verdict {
    pub(crate) fn score(&self, at: usize) -> Option<f32> {
        self.scores.get(at).copied()
    }
}

/// Pick the best of `fits` (one per candidate) with `vote` added. Ties go to
/// the earlier candidate. `decided_by` is [`MatchDecidedBy::NfoVote`] when the
/// winner did not lead on fit alone. `None` for no candidates.
pub(crate) fn decide(fits: &[f32], vote: Option<usize>) -> Option<Verdict> {
    let scores: Vec<f32> = fits
        .iter()
        .enumerate()
        .map(|(i, f)| if vote == Some(i) { f + NFO_VOTE } else { *f })
        .collect();
    let best = |xs: &[f32], skip: Option<usize>| {
        xs.iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .fold(None::<(usize, f32)>, |acc, (i, &s)| match acc {
                Some((_, b)) if s <= b => acc,
                _ => Some((i, s)),
            })
    };
    let (chosen, top) = best(&scores, None)?;
    let runner = best(&scores, Some(chosen));
    let leads_on_fit = best(fits, None).is_some_and(|(i, f)| {
        i == chosen && fits.iter().enumerate().all(|(j, &g)| j == chosen || g < f)
    });
    let decided_by = if leads_on_fit || vote != Some(chosen) {
        MatchDecidedBy::EpisodeFit
    } else {
        MatchDecidedBy::NfoVote
    };
    Some(Verdict {
        chosen,
        runner_up: runner.map(|(i, _)| i),
        close: runner.is_some_and(|(_, s)| top - s < CLEAR_MARGIN),
        scores,
        decided_by,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn local(s: u32, e: u32, title: Option<&str>) -> LocalEpisode {
        LocalEpisode {
            season: s,
            episode: e,
            title: title.map(str::to_string),
        }
    }

    fn listed(s: u32, e: u32, title: &str) -> EpisodeListing {
        EpisodeListing {
            season: s,
            episode: e,
            title: Some(title.into()),
            aired: None,
        }
    }

    /// Battlestar Galactica: the 2004 show has four seasons, the 1978 one one.
    #[test]
    fn coverage_tells_apart_shows_with_different_runs() {
        let files: Vec<_> = (1..=4).map(|s| local(s, 1, None)).collect();
        let old = [listed(1, 1, "Saga of a Star World")];
        let new: Vec<_> = (1..=4).map(|s| listed(s, 1, "x")).collect();
        assert!((fit(&files, &old) - 0.25).abs() < 1e-6);
        assert!((fit(&files, &new) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn filename_titles_tell_apart_shows_with_the_same_numbers() {
        let files = [
            local(1, 1, Some("Pilot")),
            local(1, 2, Some("Fastest Man Alive")),
        ];
        let old = [listed(1, 1, "Pilot"), listed(1, 2, "Out of Control")];
        let new = [listed(1, 1, "Pilot"), listed(1, 2, "Fastest Man Alive")];
        assert!((fit(&files, &old) - 0.75).abs() < 1e-6);
        assert!((fit(&files, &new) - 1.0).abs() < 1e-6);
        assert_eq!(fit(&[], &new), 0.0);
    }

    #[test]
    fn the_nfo_votes_by_id_then_by_a_unique_year() {
        let cands = [
            Candidate {
                id: "71173".into(),
                year: Some(1978),
            },
            Candidate {
                id: "73545".into(),
                year: Some(2004),
            },
        ];
        let by_id = ShowNfoEvidence {
            tvdb: Some("73545".into()),
            tmdb: None,
            year: Some(1978),
        };
        assert_eq!(nfo_vote("tvdb", &cands, Some(&by_id)), Some(1));
        assert_eq!(nfo_vote("tmdb", &cands, Some(&by_id)), Some(0), "year");
        let no_year_match = ShowNfoEvidence {
            year: Some(1990),
            ..ShowNfoEvidence::default()
        };
        assert_eq!(nfo_vote("tvdb", &cands, Some(&no_year_match)), None);
        assert_eq!(nfo_vote("tvdb", &cands, None), None);
    }

    #[test]
    fn a_clear_fit_wins_by_episode_fit() {
        let v = decide(&[0.25, 1.0], Some(0)).unwrap();
        assert_eq!(v.chosen, 1);
        assert_eq!(v.runner_up, Some(0));
        assert_eq!(v.decided_by, MatchDecidedBy::EpisodeFit);
        assert!(!v.close);
        assert_eq!(v.score(0), Some(0.35));
    }

    #[test]
    fn the_vote_breaks_an_even_fit_and_the_call_is_close() {
        let v = decide(&[0.5, 0.5], Some(1)).unwrap();
        assert_eq!(v.chosen, 1);
        assert_eq!(v.decided_by, MatchDecidedBy::NfoVote);
        assert!(v.close);
        let v = decide(&[0.5, 0.5], None).unwrap();
        assert_eq!(v.chosen, 0, "an even fit with no vote keeps the first");
        assert_eq!(v.decided_by, MatchDecidedBy::EpisodeFit);
        assert!(v.close);
        assert!(decide(&[], None).is_none());
    }
}
