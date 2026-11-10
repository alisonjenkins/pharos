//! The Favourites page (spec 012 US11, B1083): what the viewer has marked a
//! favourite, a rail per kind, across every library. Pure: the app root
//! loads [`Favourites`] and this draws it, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;
use std::collections::HashSet;

use pharos_client::api::{Item, ItemKind, ItemRef, Page};

use super::views::Rail;

/// How many of each kind the page asks for.
pub const PER_KIND: u32 = 60;

/// The kinds the page lists, in the order it shows them, with each rail's
/// title.
pub const KINDS: [(ItemKind, &str); 6] = [
    (ItemKind::Movie, "Films"),
    (ItemKind::Series, "Shows"),
    (ItemKind::Episode, "Episodes"),
    (ItemKind::Artist, "Artists"),
    (ItemKind::Album, "Albums"),
    (ItemKind::Audio, "Songs"),
];

/// One loaded rail per entry of [`KINDS`], in that order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Favourites {
    pub rails: Vec<FavouriteRail>,
}

/// One kind's favourites as shown.
#[derive(Debug, Clone, PartialEq)]
pub struct FavouriteRail {
    pub kind: ItemKind,
    pub items: Vec<Item>,
    /// How many the server sent, and how many it holds: a rail cut at
    /// [`PER_KIND`] says so rather than stopping without a word.
    pub loaded: usize,
    pub total: u32,
}

impl FavouriteRail {
    /// The note under a rail the server holds more of than it sent. An
    /// episode or song rail's total counts the ones folded into their show
    /// or album, so it only says there are more.
    pub fn more_note(&self) -> Option<String> {
        if self.total as usize <= self.loaded {
            return None;
        }
        let shown = self.items.len();
        Some(if folds(self.kind) {
            format!("Showing the first {shown}; there are more.")
        } else {
            format!("Showing the first {shown} of {}.", self.total)
        })
    }

    /// The note for a rail whose every loaded item folded into a favourite
    /// show or album while the server holds more.
    fn folded_note(&self) -> String {
        let into = if self.kind == ItemKind::Audio {
            "albums"
        } else {
            "shows"
        };
        format!(
            "The first {} belong to the {into} above; there are more.",
            self.loaded
        )
    }
}

/// How many more pages of episodes, or of songs, the page asks for when the
/// first ones all fold into favourite shows or albums (a whole show marked
/// a favourite fills the first page with its own episodes).
pub const MAX_EPISODE_TOP_UPS: u32 = 4;

/// Whether a kind's items can fold into a favourite group above them.
pub fn folds(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Episode | ItemKind::Audio)
}

/// The favourite shows and albums among the pages, which their episodes
/// and songs fold into: marking a show or album marks each one under it,
/// and its own tile already stands for them (B1084, B1085).
#[derive(Debug, Clone, Default)]
pub struct Folds {
    shows: HashSet<String>,
    albums: HashSet<ItemRef>,
}

impl Folds {
    pub fn of(pages: &[(ItemKind, Page)]) -> Self {
        let mut folds = Self::default();
        for (kind, page) in pages {
            for item in &page.items {
                match kind {
                    ItemKind::Series => {
                        folds.shows.insert(item.name.clone());
                    }
                    ItemKind::Album => {
                        folds.albums.insert(item.id);
                    }
                    _ => {}
                }
            }
        }
        folds
    }

    /// Whether `item` stands on its own rather than under a favourite show
    /// (an episode) or album (a song).
    pub fn stands_alone(&self, item: &Item) -> bool {
        match item.kind {
            ItemKind::Episode => !matches!(&item.series_name, Some(s) if self.shows.contains(s)),
            ItemKind::Audio => !matches!(&item.parent, Some(a) if self.albums.contains(a)),
            _ => true,
        }
    }

    /// Whether a rail of folding items should ask for its next page: it
    /// holds fewer than [`PER_KIND`] that stand alone and the server has
    /// more.
    pub fn wants_more(&self, page: &Page) -> bool {
        let alone = page
            .items
            .iter()
            .filter(|item| self.stands_alone(item))
            .count();
        alone < PER_KIND as usize && (page.items.len() as u32) < page.total
    }
}

impl Favourites {
    /// The pages as loaded, with the episodes of a favourite show and the
    /// songs of a favourite album left out ([`Folds`]). At most
    /// [`PER_KIND`] of each kind are kept.
    pub fn new(pages: Vec<(ItemKind, Page)>) -> Self {
        let folds = Folds::of(&pages);
        let rails = pages
            .into_iter()
            .map(|(kind, page)| {
                let loaded = page.items.len();
                let mut items = page.items;
                items.retain(|item| folds.stands_alone(item));
                let cut = items.len() > PER_KIND as usize;
                items.truncate(PER_KIND as usize);
                FavouriteRail {
                    kind,
                    items,
                    // Items past the cut were loaded but not shown.
                    loaded: if cut { 0 } else { loaded },
                    total: page.total,
                }
            })
            .collect();
        Self { rails }
    }

    fn is_empty(&self) -> bool {
        self.rails.iter().all(|rail| rail.items.is_empty())
    }
}

fn title_of(kind: ItemKind) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or("Other", |(_, title)| title)
}

#[component]
pub fn FavouritesView(favourites: Favourites) -> Element {
    rsx! {
        section { class: "pharos-favourites",
            h2 { "Favourites" }
            if favourites.is_empty() {
                p { class: "pharos-muted",
                    "Nothing here yet. Press Favourite on a film, show, episode, artist, album or song and it shows up here."
                }
            }
            // One pass, so every rail keeps its place in [`KINDS`].
            for rail in favourites.rails.iter().filter(|rail| !rail.items.is_empty() || rail.more_note().is_some()) {
                div { key: "{title_of(rail.kind)}", class: "pharos-favourites-kind",
                    if rail.items.is_empty() {
                        // Every episode or song read so far folded into a
                        // favourite show or album, and the server holds
                        // more: say so rather than drop the rail unseen.
                        section { class: "pharos-rail",
                            h2 { class: "pharos-rail-title", "{title_of(rail.kind)}" }
                            p { class: "pharos-muted", "{rail.folded_note()}" }
                        }
                    } else {
                        Rail {
                            title: title_of(rail.kind).to_owned(),
                            link: None,
                            items: rail.items.clone(),
                        }
                        if let Some(note) = rail.more_note() {
                            p { class: "pharos-muted", "{note}" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::route::Route;
    use crate::native::views::Nav;
    use pharos_client::api::ItemRef;

    fn item(id: u64, kind: ItemKind, name: &str, series: Option<&str>) -> Item {
        Item {
            id: ItemRef::Real(id),
            kind,
            name: name.into(),
            year: None,
            runtime_ms: 0,
            image: None,
            played: false,
            favorite: true,
            position_ms: 0,
            series_name: series.map(Into::into),
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
        }
    }

    fn page(items: Vec<Item>) -> Page {
        Page {
            total: items.len() as u32,
            items,
            start: 0,
        }
    }

    fn render(favourites: Favourites) -> String {
        let mut dom = VirtualDom::new_with_props(
            |favourites: Favourites| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                rsx! { FavouritesView { favourites } }
            },
            favourites,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// B1083: each kind with favourites gets a rail under its own title;
    /// an episode of a favourite show is left to the show's tile, while a
    /// lone favourite episode of another show stays.
    #[test]
    fn favourites_list_by_kind_and_fold_a_shows_episodes_into_it() {
        let favourites = Favourites::new(vec![
            (
                ItemKind::Movie,
                page(vec![item(1, ItemKind::Movie, "Alien", None)]),
            ),
            (
                ItemKind::Series,
                page(vec![item(2, ItemKind::Series, "Frieren", None)]),
            ),
            (
                ItemKind::Episode,
                page(vec![
                    item(3, ItemKind::Episode, "Journey's End", Some("Frieren")),
                    item(4, ItemKind::Episode, "Pilot", Some("Firefly")),
                ]),
            ),
            (ItemKind::Audio, page(vec![])),
        ]);
        let html = render(favourites);
        for title in [
            ">Films<",
            ">Shows<",
            ">Episodes<",
            "Alien",
            "Frieren",
            "Pilot",
        ] {
            assert!(html.contains(title), "{title}: {html}");
        }
        assert!(!html.contains("Journey"), "folded into its show: {html}");
        assert!(!html.contains(">Songs<"), "no songs, no rail: {html}");
        assert!(!html.contains("Nothing here yet"), "{html}");
        assert!(
            !html.contains("Showing the first"),
            "a folded episode is no missing one: {html}"
        );
    }

    /// B1084: a favourite show's episodes filling the first page do not
    /// hide a lone favourite episode further on: the page asks for more
    /// while fewer than a rail's worth stand alone and the server has more.
    #[test]
    fn episodes_folded_into_shows_ask_for_the_next_page() {
        let shows = Folds::of(&[(
            ItemKind::Series,
            page(vec![item(2, ItemKind::Series, "Frieren", None)]),
        )]);
        let mut first = page(vec![
            item(3, ItemKind::Episode, "E1", Some("Frieren")),
            item(4, ItemKind::Episode, "E2", Some("Frieren")),
        ]);
        first.total = 3;
        assert!(shows.wants_more(&first));
        first
            .items
            .push(item(5, ItemKind::Episode, "Pilot", Some("Firefly")));
        assert!(!shows.wants_more(&first), "all loaded");

        let html = render(Favourites::new(vec![
            (
                ItemKind::Series,
                page(vec![item(2, ItemKind::Series, "Frieren", None)]),
            ),
            (ItemKind::Episode, first),
        ]));
        assert!(html.contains("Pilot") && !html.contains("E1"), "{html}");
        assert!(!html.contains("Showing the first"), "{html}");
    }

    /// When every episode read folds into a favourite show and the server
    /// holds more, the rail still says there are more.
    #[test]
    fn an_episode_rail_folded_away_still_says_there_are_more() {
        let mut episodes = page(vec![item(3, ItemKind::Episode, "E1", Some("Frieren"))]);
        episodes.total = 400;
        let html = render(Favourites::new(vec![
            (
                ItemKind::Series,
                page(vec![item(2, ItemKind::Series, "Frieren", None)]),
            ),
            (ItemKind::Episode, episodes),
            (
                ItemKind::Audio,
                page(vec![item(6, ItemKind::Audio, "Zanarkand", None)]),
            ),
        ]));
        assert!(
            html.contains("The first 1 belong to the shows above; there are more."),
            "{html}"
        );
        let at: Vec<usize> = [">Shows<", ">Episodes<", ">Songs<"]
            .iter()
            .filter_map(|title| html.find(title))
            .collect();
        assert!(
            at.len() == 3 && at[0] < at[1] && at[1] < at[2],
            "every rail shows, in the page's order: {html}"
        );
        assert!(!html.contains("Nothing here yet"), "{html}");
    }

    #[test]
    fn a_cut_episode_rail_says_there_are_more() {
        let mut episodes = page(vec![item(5, ItemKind::Episode, "Pilot", Some("Firefly"))]);
        episodes.total = 400;
        let html = render(Favourites::new(vec![(ItemKind::Episode, episodes)]));
        assert!(
            html.contains("Showing the first 1; there are more."),
            "{html}"
        );
    }

    /// A rail the server holds more of than it sent says how many it shows.
    #[test]
    fn a_cut_rail_says_it_is_cut() {
        let mut films = page(vec![item(1, ItemKind::Movie, "Alien", None)]);
        films.total = 75;
        let html = render(Favourites::new(vec![(ItemKind::Movie, films)]));
        assert!(html.contains("Showing the first 1 of 75."), "{html}");
    }

    fn song(id: u64, name: &str, album: u64) -> Item {
        Item {
            parent: Some(ItemRef::Real(album)),
            ..item(id, ItemKind::Audio, name, None)
        }
    }

    /// B1085: favourite artists and albums get rails of their own, between
    /// the episodes and the songs; a song of a favourite album is left to
    /// the album's tile, while a lone favourite song stays.
    #[test]
    fn favourite_albums_and_artists_get_rails_and_fold_their_songs() {
        let html = render(Favourites::new(vec![
            (
                ItemKind::Artist,
                page(vec![item(10, ItemKind::Artist, "Massive Attack", None)]),
            ),
            (
                ItemKind::Album,
                page(vec![item(11, ItemKind::Album, "Mezzanine", None)]),
            ),
            (
                ItemKind::Audio,
                page(vec![
                    song(12, "Teardrop", 11),
                    song(13, "Unfinished Sympathy", 99),
                ]),
            ),
        ]));
        let at: Vec<usize> = [">Artists<", ">Albums<", ">Songs<"]
            .iter()
            .filter_map(|title| html.find(title))
            .collect();
        assert!(
            at.len() == 3 && at[0] < at[1] && at[1] < at[2],
            "every rail shows, in the page's order: {html}"
        );
        assert!(
            html.contains("Massive Attack") && html.contains("Mezzanine"),
            "{html}"
        );
        assert!(
            html.contains("Unfinished Sympathy"),
            "a lone song stays: {html}"
        );
        assert!(!html.contains("Teardrop"), "folded into its album: {html}");
    }

    /// Songs that all fold into favourite albums ask for the next page, and
    /// a rail left with none says where they went.
    #[test]
    fn songs_folded_into_albums_ask_for_more_and_say_so() {
        let albums = vec![(
            ItemKind::Album,
            page(vec![item(11, ItemKind::Album, "Mezzanine", None)]),
        )];
        let mut songs = page(vec![song(12, "Teardrop", 11)]);
        songs.total = 200;
        assert!(Folds::of(&albums).wants_more(&songs));
        let mut pages = albums;
        pages.push((ItemKind::Audio, songs));
        let html = render(Favourites::new(pages));
        assert!(
            html.contains(">Songs<")
                && html.contains("The first 1 belong to the albums above; there are more."),
            "{html}"
        );
    }

    #[test]
    fn no_favourites_says_how_to_add_one() {
        let html = render(Favourites::new(vec![(ItemKind::Movie, page(vec![]))]));
        assert!(html.contains("Nothing here yet"), "{html}");
        assert!(!html.contains("pharos-rail"), "{html}");
    }
}
