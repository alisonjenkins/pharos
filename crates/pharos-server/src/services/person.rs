//! A person's page: who they are, and the films, shows and episodes on this
//! server they act in that the viewer may see.

use std::collections::HashMap;

use pharos_core::{
    DomainError, ItemPerson, MediaItem, MediaKind, MediaStore as _, Person, PersonKind,
    PersonStore as _, User, UserDataStore as _, UserItemData,
};

use crate::state::AppState;

/// One item a person appears in.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    pub item: MediaItem,
    pub user_data: UserItemData,
    /// Who they play; `None` when the credit names no one.
    pub character: Option<String>,
    pub kind: PersonKind,
}

/// The episodes of one show a person appears in.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowAppearances {
    /// The show's synthesised wire id, as its episodes' `SeriesId` names it.
    pub series_id: String,
    pub series_name: String,
    /// Who they play in it, most episodes first.
    pub characters: Vec<String>,
    /// In season and episode order.
    pub episodes: Vec<Appearance>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersonPage {
    pub person: Person,
    /// Newest first.
    pub films: Vec<Appearance>,
    /// The show they are in most first.
    pub shows: Vec<ShowAppearances>,
    /// Their biography and external ids, as a metadata provider knows them.
    pub details: Option<pharos_core::PersonDetails>,
}

/// Why there is no page to show.
#[derive(Debug)]
pub enum PersonError {
    /// No person has the id.
    NotFound,
    /// Everything they appear in is withheld from the viewer: answered as
    /// an id that names nothing, so the id says nothing about the library.
    Hidden,
    Store(DomainError),
}

/// The `outcome` label of `pharos_person_page_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// At least one film or episode listed.
    Shown,
    /// A person whose credits are all crew, or on nothing still stored.
    Empty,
    NotFound,
    Hidden,
    StoreError,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Shown => "shown",
            Outcome::Empty => "empty",
            Outcome::NotFound => "not_found",
            Outcome::Hidden => "hidden",
            Outcome::StoreError => "store_error",
        }
    }
}

/// The page for the person whose wire id is `wire_id`, as `user` may see it.
/// Counted under `pharos_person_page_total{outcome}`.
pub async fn page(state: &AppState, user: &User, wire_id: &str) -> Result<PersonPage, PersonError> {
    let result = load(state, user, wire_id).await;
    let outcome = match &result {
        Ok(page) if page.films.is_empty() && page.shows.is_empty() => Outcome::Empty,
        Ok(_) => Outcome::Shown,
        Err(PersonError::NotFound) => Outcome::NotFound,
        Err(PersonError::Hidden) => Outcome::Hidden,
        Err(PersonError::Store(_)) => Outcome::StoreError,
    };
    metrics::counter!("pharos_person_page_total", "outcome" => outcome.label()).increment(1);
    match &result {
        Ok(page) => tracing::debug!(
            person.wire_id = wire_id,
            outcome = outcome.label(),
            films = page.films.len(),
            shows = page.shows.len(),
            episodes = page.shows.iter().map(|s| s.episodes.len()).sum::<usize>(),
            "person page"
        ),
        Err(PersonError::Store(e)) => {
            tracing::warn!(person.wire_id = wire_id, outcome = outcome.label(), error = %e, "person page")
        }
        Err(_) => tracing::info!(
            person.wire_id = wire_id,
            outcome = outcome.label(),
            "person page"
        ),
    }
    result
}

async fn load(state: &AppState, user: &User, wire_id: &str) -> Result<PersonPage, PersonError> {
    let person = state
        .stores
        .person_by_wire_id(wire_id)
        .await
        .map_err(PersonError::Store)?
        .ok_or(PersonError::NotFound)?;
    let credits = state
        .stores
        .credits_for_person(wire_id)
        .await
        .map_err(PersonError::Store)?;
    let mut mq = pharos_core::MediaQuery {
        person_wire_id: Some(wire_id.to_owned()),
        kinds: vec![MediaKind::Movie, MediaKind::Episode],
        ..pharos_core::MediaQuery::default()
    };
    let unscoped = mq.clone();
    crate::api::jellyfin::items::apply_policy_scope(&mut mq, &user.policy, &state.parental_ratings);
    let (items, _total) = state.stores.query(&mq).await.map_err(PersonError::Store)?;
    let acting: std::collections::HashSet<u64> = credits
        .iter()
        .filter(|(_, c)| acts(c))
        .map(|(id, _)| *id)
        .collect();
    let sees_acting = items.iter().any(|i| acting.contains(&i.id));
    if !sees_acting && !acting.is_empty() && !crate::services::library::hides_nothing(&user.policy)
    {
        let (all, _total) = state
            .stores
            .query(&unscoped)
            .await
            .map_err(PersonError::Store)?;
        if all.iter().any(|i| acting.contains(&i.id)) {
            let kind = crate::services::library::EntityKind::Person.label();
            metrics::counter!("pharos_entity_hidden_total", "kind" => kind).increment(1);
            tracing::warn!(user = %user.name, kind, wire_id, "refused an entity with no visible item");
            return Err(PersonError::Hidden);
        }
    }
    let ids: Vec<u64> = items.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(PersonError::Store)?;
    let seen = items.into_iter().zip(
        user_data
            .into_iter()
            .chain(std::iter::repeat(UserItemData::default())),
    );
    let (films, shows) = arrange(&credits, seen);
    let details = crate::person_details::current(state, &person).await;
    Ok(PersonPage {
        person,
        films,
        shows,
        details,
    })
}

/// Whether the person `wire_id` acts in something `user` may see: what
/// decides that their page is not withheld.
pub async fn acts_in_sight(
    state: &AppState,
    user: &User,
    wire_id: &str,
) -> Result<bool, pharos_core::DomainError> {
    let credits = state.stores.credits_for_person(wire_id).await?;
    let acting: std::collections::HashSet<u64> = credits
        .iter()
        .filter(|(_, c)| acts(c))
        .map(|(id, _)| *id)
        .collect();
    if acting.is_empty() || crate::services::library::hides_nothing(&user.policy) {
        return Ok(!acting.is_empty());
    }
    let mut mq = pharos_core::MediaQuery {
        person_wire_id: Some(wire_id.to_owned()),
        kinds: vec![MediaKind::Movie, MediaKind::Episode],
        ..pharos_core::MediaQuery::default()
    };
    crate::api::jellyfin::items::apply_policy_scope(&mut mq, &user.policy, &state.parental_ratings);
    let ids = state.stores.query_ids(&mq).await?;
    Ok(ids.iter().any(|id| acting.contains(id)))
}

/// Whether a credit is an acting one: the page lists what they act in.
fn acts(credit: &ItemPerson) -> bool {
    matches!(credit.kind, PersonKind::Actor | PersonKind::GuestStar)
}

/// Who a credit says they play; `None` for a blank name.
fn character(credit: &ItemPerson) -> Option<&str> {
    credit.character.as_deref().filter(|c| !c.trim().is_empty())
}

/// `items` (with the viewer's play state) that `credits` names as acting
/// work, as films and shows. An item with both a regular and a guest credit
/// counts as regular; of several credits the first naming a character wins.
pub fn arrange(
    credits: &[(u64, ItemPerson)],
    items: impl IntoIterator<Item = (MediaItem, UserItemData)>,
) -> (Vec<Appearance>, Vec<ShowAppearances>) {
    let mut credit_of: HashMap<u64, &ItemPerson> = HashMap::new();
    for (id, credit) in credits.iter().filter(|(_, c)| acts(c)) {
        let better = credit_of.get(id).map_or(true, |held| {
            let rank = |c: &ItemPerson| (c.kind == PersonKind::Actor, character(c).is_some());
            rank(credit) > rank(held)
        });
        if better {
            credit_of.insert(*id, credit);
        }
    }
    let mut films = Vec::new();
    let mut shows: Vec<ShowAppearances> = Vec::new();
    for (item, user_data) in items {
        let Some(credit) = credit_of.get(&item.id) else {
            continue;
        };
        let appearance = Appearance {
            character: character(credit).map(str::to_owned),
            kind: credit.kind,
            user_data,
            item,
        };
        match appearance.item.series.as_ref() {
            Some(series) if appearance.item.kind == MediaKind::Episode => {
                let series_id = pharos_jellyfin_api::dto::series_id_for_key(
                    series.identity_folder(),
                    &series.series_name,
                );
                match shows.iter_mut().find(|s| s.series_id == series_id) {
                    Some(show) => show.episodes.push(appearance),
                    None => shows.push(ShowAppearances {
                        series_id,
                        series_name: series.series_name.clone(),
                        characters: Vec::new(),
                        episodes: vec![appearance],
                    }),
                }
            }
            _ if appearance.item.kind == MediaKind::Movie => films.push(appearance),
            _ => {}
        }
    }
    films.sort_by(|a, b| {
        year(&b.item)
            .cmp(&year(&a.item))
            .then_with(|| a.item.title.cmp(&b.item.title))
    });
    for show in &mut shows {
        show.episodes.sort_by_key(|e| {
            let s = e.item.series.as_ref();
            (
                s.and_then(|s| s.season_number).unwrap_or(u32::MAX),
                s.and_then(|s| s.episode_number).unwrap_or(u32::MAX),
                e.item.id,
            )
        });
        show.characters = characters(&show.episodes);
    }
    shows.sort_by(|a, b| {
        b.episodes
            .len()
            .cmp(&a.episodes.len())
            .then_with(|| a.series_name.cmp(&b.series_name))
    });
    (films, shows)
}

fn year(item: &MediaItem) -> Option<u32> {
    item.metadata.production_year
}

/// The characters across `episodes`, most episodes first, then in order of
/// first appearance.
fn characters(episodes: &[Appearance]) -> Vec<String> {
    let mut counted: Vec<(String, usize)> = Vec::new();
    for name in episodes.iter().filter_map(|e| e.character.as_deref()) {
        match counted.iter_mut().find(|(n, _)| n == name) {
            Some((_, count)) => *count = count.saturating_add(1),
            None => counted.push((name.to_owned(), 1)),
        }
    }
    counted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counted.into_iter().map(|(name, _)| name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_core::SeriesInfo;

    fn credit(kind: PersonKind, character: Option<&str>) -> ItemPerson {
        ItemPerson {
            name: "Jane Lead".into(),
            wire_id: pharos_core::person_wire_id("Jane Lead"),
            kind,
            character: character.map(str::to_owned),
            ..ItemPerson::default()
        }
    }

    fn film(id: u64, title: &str, year: u32) -> MediaItem {
        let mut item = MediaItem {
            id,
            kind: MediaKind::Movie,
            title: title.into(),
            ..MediaItem::default()
        };
        item.metadata.production_year = Some(year);
        item
    }

    fn episode(id: u64, show: &str, season: u32, number: u32) -> MediaItem {
        MediaItem {
            id,
            kind: MediaKind::Episode,
            title: format!("{show} {season}x{number}"),
            series: Some(SeriesInfo {
                series_name: show.into(),
                season_number: Some(season),
                episode_number: Some(number),
                series_folder: Some(format!("/tv/{show}")),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        }
    }

    fn seen(items: Vec<MediaItem>) -> Vec<(MediaItem, UserItemData)> {
        items
            .into_iter()
            .map(|i| (i, UserItemData::default()))
            .collect()
    }

    #[test]
    fn films_come_newest_first_with_who_they_play() {
        let credits = [
            (1, credit(PersonKind::Actor, Some("Ripley"))),
            (2, credit(PersonKind::Actor, None)),
        ];
        let (films, shows) = arrange(
            &credits,
            seen(vec![film(1, "Film AX", 1979), film(2, "Film AW", 1986)]),
        );
        assert!(shows.is_empty());
        let names: Vec<_> = films.iter().map(|f| f.item.title.as_str()).collect();
        assert_eq!(names, ["Film AW", "Film AX"]);
        assert_eq!(films[1].character.as_deref(), Some("Ripley"));
        assert_eq!(films[0].character, None);
    }

    #[test]
    fn episodes_group_under_their_show_in_season_order() {
        let credits = [
            (10, credit(PersonKind::Actor, Some("Captain"))),
            (11, credit(PersonKind::Actor, Some("Captain"))),
            (12, credit(PersonKind::Actor, Some("Captain (voice)"))),
            (20, credit(PersonKind::GuestStar, Some("Stranger"))),
        ];
        let items = vec![
            episode(12, "Voyage", 2, 1),
            episode(20, "Other Show", 1, 3),
            episode(11, "Voyage", 1, 2),
            episode(10, "Voyage", 1, 1),
        ];
        let (films, shows) = arrange(&credits, seen(items));
        assert!(films.is_empty());
        assert_eq!(shows.len(), 2);
        assert_eq!(
            shows[0].series_name, "Voyage",
            "the show with most episodes first"
        );
        let order: Vec<_> = shows[0].episodes.iter().map(|e| e.item.id).collect();
        assert_eq!(order, [10, 11, 12]);
        assert_eq!(shows[0].characters, ["Captain", "Captain (voice)"]);
        assert_eq!(
            shows[0].series_id,
            pharos_jellyfin_api::dto::series_id_for_key(Some("/tv/Voyage"), "Voyage")
        );
        assert_eq!(shows[1].episodes[0].kind, PersonKind::GuestStar);
    }

    #[test]
    fn crew_credits_and_uncredited_items_are_left_out() {
        let mut director = credit(PersonKind::Director, None);
        director.role = Some("Director".into());
        let credits = [(1, director), (2, credit(PersonKind::Actor, Some("Lead")))];
        let (films, _) = arrange(
            &credits,
            seen(vec![
                film(1, "Directed", 2000),
                film(2, "Acted", 2001),
                film(3, "Neither", 2002),
            ]),
        );
        let names: Vec<_> = films.iter().map(|f| f.item.title.as_str()).collect();
        assert_eq!(names, ["Acted"]);
    }

    #[test]
    fn a_regular_credit_outranks_a_guest_one_on_the_same_item() {
        let credits = [
            (1, credit(PersonKind::GuestStar, Some("Cameo"))),
            (1, credit(PersonKind::Actor, Some("Lead"))),
        ];
        let (films, _) = arrange(&credits, seen(vec![film(1, "Both", 2000)]));
        assert_eq!(films[0].kind, PersonKind::Actor);
        assert_eq!(films[0].character.as_deref(), Some("Lead"));
    }

    #[test]
    fn a_named_character_outranks_a_blank_one_on_the_same_item() {
        let credits = [
            (1, credit(PersonKind::Actor, Some(" "))),
            (1, credit(PersonKind::Actor, Some("Ripley"))),
        ];
        let (films, _) = arrange(&credits, seen(vec![film(1, "Film AX", 1979)]));
        assert_eq!(films[0].character.as_deref(), Some("Ripley"));
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let labels: std::collections::HashSet<_> = [
            Outcome::Shown,
            Outcome::Empty,
            Outcome::NotFound,
            Outcome::Hidden,
            Outcome::StoreError,
        ]
        .map(Outcome::label)
        .into_iter()
        .collect();
        assert_eq!(labels.len(), 5);
    }
}
