//! The native `Library.search`: the shows, artists and albums a term names
//! ahead of the stored rows that match it, and a trailing `s01` or `s01e02`
//! read as a season or an episode of the show named before it (B597/V518).

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

/// How well a name holds a search term, best first: the whole name, its
/// start, the start of a later word, anywhere. Shorter names come first
/// within a tier, so "Dragon Ball" leads "Dragon Ball GT".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NameMatch {
    tier: u8,
    len: usize,
}

impl NameMatch {
    pub fn is_exact(self) -> bool {
        self.tier == 0
    }
}

/// How `name` holds `term` (any case), or `None` when it does not.
pub fn name_match(name: &str, term: &str) -> Option<NameMatch> {
    let name = name.to_lowercase();
    let term = term.trim().to_lowercase();
    if term.is_empty() {
        return None;
    }
    let at = name.find(&term)?;
    let tier = if name == term {
        0
    } else if at == 0 {
        1
    } else if name
        .match_indices(&term)
        .any(|(at, _)| !name[..at].ends_with(char::is_alphanumeric))
    {
        2
    } else {
        3
    };
    Some(NameMatch {
        tier,
        len: name.chars().count(),
    })
}

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
    Stored(Box<MediaItem>),
}

impl Hit {
    fn group(dto: serde_json::Value) -> Option<Self> {
        let id = WireId::parse(dto.get("Id")?.as_str()?)?;
        Some(Hit::Group(id, dto))
    }
}

/// What `text` names that `user` may see, at most `limit` rows (0 is
/// [`library::SEARCH_DEFAULT`]): for a name of [`GROUP_MIN_CHARS`] or more,
/// the shows it names, or with a season or episode asked for that season or
/// episode of each; then the artists and albums it names; then the stored
/// rows `text` matches. A lookup that fails is logged and left out, so the
/// stored rows still answer.
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
    let mut hits = Vec::new();
    if term.name.chars().count() >= GROUP_MIN_CHARS {
        hits.extend(show_hits(state, user, &term).await);
        if term.season.is_none() {
            match crate::api::jellyfin::items::music_groups_named(
                state,
                user,
                &term.name,
                GROUP_LIMIT,
            )
            .await
            {
                Ok((artists, albums)) => {
                    hits.extend(artists.into_iter().chain(albums).filter_map(Hit::group))
                }
                Err(error) => tracing::warn!(%error, "search: artists and albums unavailable"),
            }
        }
    }
    let stored = library::search(state, user, text, limit as u32).await?;
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

/// The shows named by `term`, best named first; or, when it asks for one,
/// that season or episode of each. Only the shows kept are built into
/// tiles: a common word such as "the" names a hundred or more.
async fn show_hits(state: &AppState, user: &User, term: &Term) -> Vec<Hit> {
    use pharos_jellyfin_api::dto::series_id_for_key;
    let shows = match crate::api::jellyfin::items::shows_matching(state, user, &term.name).await {
        Ok(shows) => shows,
        Err(error) => {
            tracing::warn!(%error, "search: shows unavailable");
            return Vec::new();
        }
    };
    // The store matched episode titles as well; only a show's own name counts.
    let mut shows: Vec<_> = shows
        .into_iter()
        .filter_map(|(folder, name)| Some((name_match(&name, &term.name)?, name, folder)))
        .collect();
    shows.sort();
    shows.truncate(GROUP_LIMIT);
    let mut hits = Vec::new();
    for (_, name, folder) in shows {
        let Some(id) = WireId::parse(&series_id_for_key(folder.as_deref(), &name)) else {
            continue;
        };
        let hit = match (term.season, term.episode) {
            (None, _) => show_tile(state, user, id, &name).await,
            (Some(season), None) => season_of(state, user, id, season).await,
            (Some(season), Some(episode)) => episode_of(state, user, id, season, episode).await,
        };
        hits.extend(hit);
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
        let rank = |name| name_match(name, "Dragon Ball");
        assert!(rank("Dragon Ball").unwrap().is_exact());
        assert!(rank("Dragon Ball").unwrap() < rank("Dragon Ball GT").unwrap());
        assert!(rank("Dragon Ball GT").unwrap() < rank("Dragon Ball Super").unwrap());
        assert!(rank("Dragon Ball Super").unwrap() < rank("Super Dragon Ball Heroes").unwrap());
        assert!(rank("Super Dragon Ball Heroes").unwrap() < rank("SuperDragon Balls").unwrap());
        assert_eq!(rank("Bleach"), None);
    }
}
