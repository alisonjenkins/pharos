//! The native `Library.search`: the shows, artists, albums and actors a term names
//! ahead of the stored rows that match it, and a trailing `s01` or `s01e02`
//! read as a season or an episode of the show named before it (B597/V518).
//! A name is found word by word, in any order and across a show's, an
//! artist's or an album's names, best named first; and when nothing is
//! found, again allowing a typo in each word (B624/V558).

use pharos_core::fuzzy::{score, Score};
use pharos_core::{MediaItem, MediaKind, User};
use pharos_jellyfin_api::dto::WireId;

use crate::services::library::{self, Grouping, ItemQuery, KindFilter};
use crate::state::AppState;

/// The fewest characters a name needs before shows, artists and albums are
/// looked for: each lookup folds every stored row the name matches, and one
/// or two letters match most of the library on every keystroke.
pub const GROUP_MIN_CHARS: usize = 3;

/// The most shows, artists or albums one search lists of each.
pub const GROUP_LIMIT: usize = 5;

/// A search term split into the name it searches for and, when it ends in
/// one, the season (`s1`, `s01`) or episode (`s01e02`, `s1 e2`) it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub name: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

impl Term {
    pub fn parse(text: &str) -> Self {
        let words: Vec<&str> = text.split_whitespace().collect();
        let plain = Term {
            name: words.join(" "),
            season: None,
            episode: None,
        };
        let numbered = |tail: &[&str]| -> Option<(u32, Option<u32>)> {
            match tail {
                [one] => {
                    let rest = strip_letter(one, 's')?;
                    match rest.find(['e', 'E']) {
                        Some(at) => Some((number(&rest[..at])?, Some(number(&rest[at + 1..])?))),
                        None => Some((number(rest)?, None)),
                    }
                }
                [season, episode] => Some((
                    number(strip_letter(season, 's')?)?,
                    Some(number(strip_letter(episode, 'e')?)?),
                )),
                _ => None,
            }
        };
        for take in [2, 1] {
            let Some(split) = words.len().checked_sub(take).filter(|at| *at > 0) else {
                continue;
            };
            if let Some((season, episode)) = numbered(&words[split..]) {
                return Term {
                    name: words[..split].join(" "),
                    season: Some(season),
                    episode,
                };
            }
        }
        plain
    }
}

fn strip_letter(word: &str, letter: char) -> Option<&str> {
    word.strip_prefix(letter)
        .or_else(|| word.strip_prefix(letter.to_ascii_uppercase()))
}

/// A season or episode number: one to four digits.
fn number(digits: &str) -> Option<u32> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One row of a native search answer.
#[derive(Debug)]
pub enum Hit {
    /// A show, season, artist or album, as its Jellyfin DTO.
    Group(WireId, serde_json::Value),
    /// Someone who acts in what the viewer may see.
    Person(WireId, Box<pharos_core::Person>),
    Stored(Box<MediaItem>),
}

impl Hit {
    fn group(dto: serde_json::Value) -> Option<Self> {
        let id = WireId::parse(dto.get("Id")?.as_str()?)?;
        Some(Hit::Group(id, dto))
    }
}

/// Which pass of a search answered: words as typed, or with typos. The
/// `pass` label of `pharos_search_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    Strict,
    Loose,
}

impl Pass {
    pub fn label(self) -> &'static str {
        match self {
            Pass::Strict => "strict",
            Pass::Loose => "loose",
        }
    }
}

/// What `text` names that `user` may see, at most `limit` rows (0 is
/// [`library::SEARCH_DEFAULT`]): for a name of [`GROUP_MIN_CHARS`] or more,
/// the shows, artists and albums holding every word of it, best named first
/// (or, with a season or episode asked for, that season or episode of each
/// show); then the stored rows `text` matches. When that finds nothing, the
/// shows, artists and albums are looked for again allowing a typo in each
/// word. A lookup that fails is logged and left out, so the stored rows
/// still answer.
pub async fn search(
    state: &AppState,
    user: &User,
    text: &str,
    limit: u32,
) -> Result<Vec<Hit>, pharos_core::DomainError> {
    let limit = match limit {
        0 => library::SEARCH_DEFAULT,
        n => n.min(library::QUERY_MAX),
    } as usize;
    let term = Term::parse(text);
    let mut hits = group_hits(state, user, &term, Pass::Strict).await;
    let stored = library::search(state, user, text, limit as u32).await?;
    let mut pass = Pass::Strict;
    if hits.is_empty() && stored.is_empty() {
        pass = Pass::Loose;
        hits = group_hits(state, user, &term, Pass::Loose).await;
    }
    let outcome = if hits.is_empty() && stored.is_empty() {
        "empty"
    } else {
        "found"
    };
    metrics::counter!("pharos_search_total", "pass" => pass.label(), "outcome" => outcome)
        .increment(1);
    tracing::debug!(
        user.id = %user.id,
        pass = pass.label(),
        outcome,
        words = pharos_core::search_tokens(&term.name).len(),
        groups = hits.len(),
        people = hits.iter().filter(|h| matches!(h, Hit::Person(..))).count(),
        stored = stored.len(),
        "search: answered"
    );
    for item in stored {
        let listed = hits
            .iter()
            .any(|hit| matches!(hit, Hit::Stored(seen) if seen.id == item.id));
        if !listed {
            hits.push(Hit::Stored(Box::new(item)));
        }
    }
    hits.truncate(limit);
    Ok(hits)
}

/// The shows, artists and albums `term` names, best named first; a show
/// leads an artist, and an artist an album, named as well. Nothing for a
/// name shorter than [`GROUP_MIN_CHARS`].
async fn group_hits(state: &AppState, user: &User, term: &Term, pass: Pass) -> Vec<Hit> {
    if term.name.chars().count() < GROUP_MIN_CHARS {
        return Vec::new();
    }
    let loose = pass == Pass::Loose;
    let mut named: Vec<(Score, Hit)> = show_hits(state, user, term, loose).await;
    if term.season.is_none() {
        match crate::api::jellyfin::items::music_groups_named(
            state,
            user,
            &term.name,
            GROUP_LIMIT,
            loose,
        )
        .await
        {
            Ok((artists, albums)) => named.extend(
                artists
                    .into_iter()
                    .chain(albums)
                    .filter_map(|(rank, dto)| Some((rank, Hit::group(dto)?))),
            ),
            Err(error) => tracing::warn!(%error, "search: artists and albums unavailable"),
        }
    }
    if !loose && term.season.is_none() {
        named.extend(actor_hits(state, user, &term.name).await);
    }
    // Stable: on a tie a show keeps its place ahead of an artist, an album
    // or an actor.
    named.sort_by_key(|(rank, _)| *rank);
    named.into_iter().map(|(_, hit)| hit).collect()
}

/// How many actors the store is asked for before ranking. The store matches
/// substrings shortest name first, so a short term ("ann") fills the list
/// with names holding it mid-word; ranking keeps only word matches.
const ACTOR_CANDIDATES: u32 = 100;

/// The actors `name` names that act in something `user` may see, best named
/// first, at most [`GROUP_LIMIT`]. A lookup that fails is logged and gives
/// none.
async fn actor_hits(state: &AppState, user: &User, name: &str) -> Vec<(Score, Hit)> {
    use pharos_core::PersonStore as _;
    let words = pharos_core::search_tokens(name);
    let people = match state.stores.actors_named(&words, ACTOR_CANDIDATES).await {
        Ok(people) => people,
        Err(error) => {
            tracing::warn!(%error, "search: actors unavailable");
            return Vec::new();
        }
    };
    let mut ranked: Vec<_> = people
        .into_iter()
        .filter_map(|p| Some((score(&[p.name.as_str()], name)?, p)))
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    if crate::services::library::hides_nothing(&user.policy) {
        return ranked
            .into_iter()
            .take(GROUP_LIMIT)
            .filter_map(|(rank, p)| {
                Some((rank, Hit::Person(WireId::parse(&p.wire_id)?, Box::new(p))))
            })
            .collect();
    }
    // Checked a few at a time, best named first, until enough are in sight.
    let mut hits = Vec::new();
    let mut ranked = ranked.into_iter();
    while hits.len() < GROUP_LIMIT {
        let batch: Vec<_> = ranked.by_ref().take(GROUP_LIMIT).collect();
        if batch.is_empty() {
            break;
        }
        let seen = futures_util::future::join_all(
            batch
                .iter()
                .map(|(_, p)| crate::services::person::acts_in_sight(state, user, &p.wire_id)),
        )
        .await;
        for ((rank, person), seen) in batch.into_iter().zip(seen) {
            match seen {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    tracing::warn!(%error, person.wire_id, "search: actor unavailable");
                    continue;
                }
            }
            if let Some(id) = WireId::parse(&person.wire_id) {
                hits.push((rank, Hit::Person(id, Box::new(person))));
            }
        }
    }
    hits.truncate(GROUP_LIMIT);
    hits
}

/// The shows named by `term`, best named first; or, when it asks for one,
/// that season or episode of each. Only the shows kept are built into
/// tiles: a common word such as "the" names a hundred or more.
async fn show_hits(state: &AppState, user: &User, term: &Term, loose: bool) -> Vec<(Score, Hit)> {
    use pharos_jellyfin_api::dto::series_id_for_key;
    let shows =
        match crate::api::jellyfin::items::shows_matching(state, user, &term.name, loose).await {
            Ok(shows) => shows,
            Err(error) => {
                tracing::warn!(%error, "search: shows unavailable");
                return Vec::new();
            }
        };
    // The store matched episode titles as well; only a show's own name counts.
    let mut shows: Vec<_> = shows
        .into_iter()
        .filter_map(|(folder, name)| Some((score(&[name.as_str()], &term.name)?, name, folder)))
        .collect();
    shows.sort();
    shows.truncate(GROUP_LIMIT);
    let mut hits = Vec::new();
    for (rank, name, folder) in shows {
        let Some(id) = WireId::parse(&series_id_for_key(folder.as_deref(), &name)) else {
            continue;
        };
        let hit = match (term.season, term.episode) {
            (None, _) => show_tile(state, user, id, &name).await,
            (Some(season), None) => season_of(state, user, id, season).await,
            (Some(season), Some(episode)) => episode_of(state, user, id, season, episode).await,
        };
        hits.extend(hit.map(|hit| (rank, hit)));
    }
    hits
}

/// Show `id`'s tile, as the TV library lists it.
async fn show_tile(state: &AppState, user: &User, id: WireId, name: &str) -> Option<Hit> {
    let q = ItemQuery {
        search_term: Some(name.to_owned()),
        ..ItemQuery::default()
    };
    let shows = library::grouped(state, user, None, Grouping::Series, &q)
        .await
        .inspect_err(|error| tracing::warn!(?error, name, "search: show unavailable"))
        .ok()?;
    shows
        .items
        .into_iter()
        .filter_map(Hit::group)
        .find(|hit| matches!(hit, Hit::Group(found, _) if *found == id))
}

async fn season_of(state: &AppState, user: &User, show: WireId, season: u32) -> Option<Hit> {
    let show = show.to_string();
    let seasons = library::grouped(
        state,
        user,
        Some(&show),
        Grouping::Season,
        &ItemQuery::default(),
    )
    .await
    .inspect_err(|error| tracing::warn!(?error, show, "search: seasons unavailable"))
    .ok()?;
    seasons
        .items
        .into_iter()
        .find(|dto| dto.get("IndexNumber").and_then(|n| n.as_u64()) == Some(u64::from(season)))
        .and_then(Hit::group)
}

async fn episode_of(
    state: &AppState,
    user: &User,
    show: WireId,
    season: u32,
    episode: u32,
) -> Option<Hit> {
    let show = show.to_string();
    let q = ItemQuery {
        kinds: KindFilter::Kinds(vec![MediaKind::Episode]),
        ..ItemQuery::default()
    };
    let episodes = library::query(state, user, Some(&show), &q)
        .await
        .inspect_err(|error| tracing::warn!(%error, show, "search: episodes unavailable"))
        .ok()?;
    episodes
        .items
        .into_iter()
        .find(|item| {
            item.series.as_ref().is_some_and(|s| {
                s.season_number == Some(season) && s.episode_number == Some(episode)
            })
        })
        .map(|item| Hit::Stored(Box::new(item)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn term(name: &str, season: Option<u32>, episode: Option<u32>) -> Term {
        Term {
            name: name.into(),
            season,
            episode,
        }
    }

    #[test]
    fn a_trailing_season_or_episode_token_is_read_off_the_name() {
        assert_eq!(
            Term::parse("dragon ball s01"),
            term("dragon ball", Some(1), None)
        );
        assert_eq!(
            Term::parse("dragon ball s1"),
            term("dragon ball", Some(1), None)
        );
        assert_eq!(
            Term::parse("Dragon Ball S01E02"),
            term("Dragon Ball", Some(1), Some(2))
        );
        assert_eq!(
            Term::parse("dragon ball s01e2"),
            term("dragon ball", Some(1), Some(2))
        );
        assert_eq!(
            Term::parse("dragon  ball s1 e2"),
            term("dragon ball", Some(1), Some(2))
        );
    }

    #[test]
    fn a_term_without_a_season_token_is_all_name() {
        assert_eq!(Term::parse("dragon ball"), term("dragon ball", None, None));
        assert_eq!(Term::parse("s01"), term("s01", None, None));
        assert_eq!(
            Term::parse("the boys season"),
            term("the boys season", None, None)
        );
        assert_eq!(
            Term::parse("sons of anarchy"),
            term("sons of anarchy", None, None)
        );
        assert_eq!(Term::parse("s1e"), term("s1e", None, None));
        assert_eq!(Term::parse("ally s01ex"), term("ally s01ex", None, None));
    }

    #[test]
    fn names_rank_whole_then_start_then_word_then_anywhere_then_shorter() {
        let rank = |name| score(&[name], "Dragon Ball");
        assert!(rank("Dragon Ball").unwrap().is_exact());
        assert!(rank("Dragon Ball").unwrap() < rank("Dragon Ball GT").unwrap());
        assert!(rank("Dragon Ball GT").unwrap() < rank("Dragon Ball Super").unwrap());
        assert!(rank("Dragon Ball Super").unwrap() < rank("Super Dragon Ball Heroes").unwrap());
        assert!(rank("Super Dragon Ball Heroes").unwrap() < rank("SuperDragon Balls").unwrap());
        assert_eq!(rank("Bleach"), None);
    }

    #[test]
    fn pass_labels_are_distinct() {
        assert_ne!(Pass::Strict.label(), Pass::Loose.label());
    }
}
