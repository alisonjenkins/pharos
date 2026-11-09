//! The Favourites page (spec 012 US11, B1083): what the viewer has marked a
//! favourite, a rail per kind, across every library. Pure: the app root
//! loads [`Favourites`] and this draws it, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;
use pharos_client::api::{Item, ItemKind, Page};

use super::views::Rail;

/// How many of each kind the page asks for.
pub const PER_KIND: u32 = 60;

/// The kinds the page lists, in the order it shows them, with each rail's
/// title.
pub const KINDS: [(ItemKind, &str); 4] = [
    (ItemKind::Movie, "Films"),
    (ItemKind::Series, "Shows"),
    (ItemKind::Episode, "Episodes"),
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
    /// The note under a rail the server holds more of than it sent.
    pub fn more_note(&self) -> Option<String> {
        (self.total as usize > self.loaded)
            .then(|| format!("Showing the first {} of {}.", self.loaded, self.total))
    }
}

impl Favourites {
    /// The pages as loaded, with the episodes of a favourite show left out:
    /// marking a show marks every episode, and the show's own tile already
    /// stands for them.
    pub fn new(pages: Vec<(ItemKind, Page)>) -> Self {
        let shows: std::collections::HashSet<String> = pages
            .iter()
            .filter(|(kind, _)| *kind == ItemKind::Series)
            .flat_map(|(_, page)| page.items.iter().map(|s| s.name.clone()))
            .collect();
        let rails = pages
            .into_iter()
            .map(|(kind, page)| {
                let loaded = page.items.len();
                let mut items = page.items;
                if kind == ItemKind::Episode {
                    items.retain(|ep| !matches!(&ep.series_name, Some(s) if shows.contains(s)));
                }
                FavouriteRail {
                    kind,
                    items,
                    loaded,
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
                    "Nothing here yet. Press Favourite on a film, show, episode or song (or f on its page) and it shows up here."
                }
            }
            for rail in favourites.rails.iter().filter(|rail| !rail.items.is_empty()) {
                Rail {
                    key: "{title_of(rail.kind)}",
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

    /// A rail the server holds more of than it sent says how many it shows.
    #[test]
    fn a_cut_rail_says_it_is_cut() {
        let mut films = page(vec![item(1, ItemKind::Movie, "Alien", None)]);
        films.total = 75;
        let html = render(Favourites::new(vec![(ItemKind::Movie, films)]));
        assert!(html.contains("Showing the first 1 of 75."), "{html}");
    }

    #[test]
    fn no_favourites_says_how_to_add_one() {
        let html = render(Favourites::new(vec![(ItemKind::Movie, page(vec![]))]));
        assert!(html.contains("Nothing here yet"), "{html}");
        assert!(!html.contains("pharos-rail"), "{html}");
    }
}
