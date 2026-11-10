//! The Ctrl+K / Cmd+K palette's rules, free of the browser: the places it
//! can go, which of them a term matches, and the rows it lists in order.

use pharos_client::api::{Item, Page};

use super::route::Route;
use super::views::open_route;

/// How many items to resume the palette offers before anything is typed.
pub const RECENT_LIMIT: usize = 5;

/// How many past searches the palette offers before anything is typed.
pub const SEARCH_LIMIT: usize = 5;

/// Where the past searches are kept, newest first, one per line.
pub const SEARCHES_KEY: &str = "pharos.search.recent";

/// A screen the palette can go to without searching.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub label: String,
    pub detail: &'static str,
    pub route: Route,
}

/// A command the palette runs rather than a screen it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ReportProblem,
}

impl Command {
    pub fn label(self) -> &'static str {
        match self {
            Command::ReportProblem => "Report a problem",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Command::ReportProblem => "Shows what would be sent first",
        }
    }
}

/// Every command, in the order the palette lists them.
pub const COMMANDS: &[Command] = &[Command::ReportProblem];

/// One row of the palette, in the group it is listed under.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Place(Place),
    Command(Command),
    /// A term searched for before; picking it searches again.
    Search(String),
    /// Something the user was part way through.
    Recent(Item),
    /// A `Library.search` match.
    Title(Item),
}

impl Row {
    /// Where picking the row goes; a past search goes nowhere, it searches.
    pub fn route(&self) -> Option<Route> {
        match self {
            Row::Place(place) => Some(place.route.clone()),
            Row::Search(_) | Row::Command(_) => None,
            Row::Recent(item) | Row::Title(item) => Some(open_route(item)),
        }
    }

    pub fn group(&self) -> &'static str {
        match self {
            Row::Place(_) => "Go to",
            Row::Command(_) => "Actions",
            Row::Search(_) => "Recent searches",
            Row::Recent(_) => "Continue",
            Row::Title(_) => "Titles",
        }
    }
}

/// Home, each library in the order given, Favourites, then the settings
/// screens.
pub fn places(libraries: &[Item]) -> Vec<Place> {
    let home = Place {
        label: "Home".into(),
        detail: "Page",
        route: Route::Home,
    };
    let libraries = libraries.iter().map(|library| Place {
        label: library.name.clone(),
        detail: "Library",
        route: Route::Browse(library.id),
    });
    let favourites = Place {
        label: "Favourites".into(),
        detail: "Page",
        route: Route::Favourites,
    };
    let settings = [
        Place {
            label: "Appearance".into(),
            detail: "Settings",
            route: Route::Appearance,
        },
        Place {
            label: "Manage".into(),
            detail: "Settings",
            route: Route::Manage,
        },
    ];
    std::iter::once(home)
        .chain(libraries)
        .chain(std::iter::once(favourites))
        .chain(settings)
        .collect()
}

/// The rows for `term`: past searches, every place and what to resume while
/// it is blank; once typed, the places whose name holds it (any case) and
/// then the search matches, which are `None` until the first reply lands.
pub fn rows(
    searches: &[String],
    places: &[Place],
    recent: &[Item],
    term: &str,
    hits: Option<&Page>,
) -> Vec<Row> {
    let term = term.trim().to_lowercase();
    if term.is_empty() {
        return searches
            .iter()
            .take(SEARCH_LIMIT)
            .cloned()
            .map(Row::Search)
            .chain(places.iter().cloned().map(Row::Place))
            .chain(COMMANDS.iter().copied().map(Row::Command))
            .chain(recent.iter().take(RECENT_LIMIT).cloned().map(Row::Recent))
            .collect();
    }
    places
        .iter()
        .filter(|place| place.label.to_lowercase().contains(&term))
        .cloned()
        .map(Row::Place)
        .chain(
            COMMANDS
                .iter()
                .copied()
                .filter(|c| c.label().to_lowercase().contains(&term))
                .map(Row::Command),
        )
        .chain(
            hits.into_iter()
                .flat_map(|page| page.items.iter().cloned().map(Row::Title)),
        )
        .collect()
}

/// The past searches kept as `stored`, newest first.
pub fn searches(stored: &str) -> Vec<String> {
    stored
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(SEARCH_LIMIT)
        .map(str::to_owned)
        .collect()
}

/// `stored` with `term` put first (once, any case), as it is kept.
pub fn remember(stored: &str, term: &str) -> String {
    let term = term.split_whitespace().collect::<Vec<_>>().join(" ");
    std::iter::once(term.clone())
        .chain(
            searches(stored)
                .into_iter()
                .filter(|t| !t.eq_ignore_ascii_case(&term)),
        )
        .take(SEARCH_LIMIT)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a key press opens or closes the palette: K with Ctrl, or with
/// Cmd (the meta key) as macOS has it.
pub fn is_toggle(key: &str, ctrl: bool, meta: bool) -> bool {
    (ctrl || meta) && key.eq_ignore_ascii_case("k")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_client::api::{ItemKind, ItemRef};

    fn item(id: u64, kind: ItemKind, name: &str) -> Item {
        Item {
            id: ItemRef::Real(id),
            kind,
            name: name.into(),
            year: None,
            runtime_ms: 0,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
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

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Place(place) => place.label.clone(),
                Row::Search(term) => term.clone(),
                Row::Command(command) => command.label().to_owned(),
                Row::Recent(item) | Row::Title(item) => item.name.clone(),
            })
            .collect()
    }

    #[test]
    fn the_places_are_home_the_libraries_favourites_then_settings() {
        let libraries = [
            item(4, ItemKind::Library, "Films"),
            item(5, ItemKind::Library, "TV"),
        ];
        let places = places(&libraries);
        let labels: Vec<_> = places.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Home", "Films", "TV", "Favourites", "Appearance", "Manage"]
        );
        assert_eq!(places[3].route, Route::Favourites);
        assert_eq!(places[1].route, Route::Browse(ItemRef::Real(4)));
    }

    #[test]
    fn a_blank_term_lists_every_place_then_a_few_to_resume() {
        let places = places(&[]);
        let recent: Vec<_> = (1..=8)
            .map(|n| item(n, ItemKind::Movie, &format!("R{n}")))
            .collect();
        let rows = rows(&[], &places, &recent, "  ", None);
        assert_eq!(rows.len(), places.len() + COMMANDS.len() + RECENT_LIMIT);
        assert!(matches!(rows.last(), Some(Row::Recent(_))));
    }

    #[test]
    fn a_term_keeps_the_places_it_names_then_the_matches() {
        let places = places(&[item(4, ItemKind::Library, "Music")]);
        let recent = [item(9, ItemKind::Movie, "Resumable")];
        let hits = Page {
            items: vec![item(7, ItemKind::Audio, "Music Box")],
            total: 1,
            start: 0,
        };
        let rows = rows(&[], &places, &recent, "MUS", Some(&hits));
        assert_eq!(labels(&rows), ["Music", "Music Box"]);
        assert_eq!(rows[1].route(), Some(Route::Title(ItemRef::Real(7))));
        assert_eq!(rows[1].group(), "Titles");
    }

    #[test]
    fn the_report_command_is_listed_and_found_by_name_and_runs_no_route() {
        let places = places(&[]);
        let blank = rows(&[], &places, &[], "", None);
        let at = blank.len().min(places.len());
        assert_eq!(blank[at], Row::Command(Command::ReportProblem));
        assert_eq!(blank[at].group(), "Actions");
        assert_eq!(blank[at].route(), None);
        assert_eq!(
            labels(&rows(&[], &places, &[], "problem", None)),
            ["Report a problem"]
        );
    }

    #[test]
    fn before_the_first_reply_only_places_are_listed() {
        let places = places(&[]);
        assert_eq!(
            labels(&rows(&[], &places, &[], "app", None)),
            ["Appearance"]
        );
    }

    #[test]
    fn a_blank_term_lists_past_searches_first_and_they_go_nowhere() {
        let places = places(&[]);
        let searches = searches(&remember(&remember("", "moby"), "silver rock"));
        let blank = rows(&searches, &places, &[], "", None);
        assert_eq!(labels(&blank[..3]), ["silver rock", "moby", "Home"]);
        assert_eq!(blank[0].group(), "Recent searches");
        assert_eq!(blank[0].route(), None);
        assert!(rows(&searches, &places, &[], "mo", None)
            .iter()
            .all(|row| !matches!(row, Row::Search(_))));
    }

    #[test]
    fn a_search_is_remembered_once_newest_first_and_only_the_last_few() {
        let kept = remember("Moby\nsignify", "  moby  ");
        assert_eq!(searches(&kept), ["moby", "signify"]);
        let kept = (0..9).fold(String::new(), |kept, n| remember(&kept, &format!("t{n}")));
        assert_eq!(searches(&kept), ["t8", "t7", "t6", "t5", "t4"]);
    }

    #[test]
    fn ctrl_or_cmd_with_k_toggles() {
        assert!(is_toggle("k", true, false));
        assert!(is_toggle("K", false, true));
        assert!(!is_toggle("k", false, false));
        assert!(!is_toggle("j", true, false));
    }
}
