//! A person's page: their photo, links out to IMDb and Wikipedia, and the
//! films, shows and episodes on this server they act in.

use dioxus::prelude::*;
use pharos_client::api::{Appearance, CreditKind, PersonPage, ShowAppearances};

use super::account::initials;
use super::browse::encode_component;
use super::keymap::Mode;
use super::keys::use_key_mode;
use super::route::Route;
use super::views::{display_name, episode_label, open_route, Link, PosterTile};

/// A show with this many episodes or fewer lists them open; a longer list
/// starts folded, so one long-running show cannot bury the rest of the page.
const OPEN_EPISODES: usize = 6;

/// Where IMDb finds people named `name`.
pub fn imdb_url(name: &str) -> String {
    format!(
        "https://www.imdb.com/find/?q={}&s=nm",
        encode_component(name)
    )
}

/// Wikipedia's article titled `name` when there is one, else its search for
/// it: `go` jumps straight to an exact title.
pub fn wikipedia_url(name: &str) -> String {
    format!(
        "https://en.wikipedia.org/w/index.php?search={}&title=Special%3ASearch&go=Go",
        encode_component(name)
    )
}

/// "In 2 films and 1 show", or `None` when they are in nothing here.
fn appears_in(films: usize, shows: usize) -> Option<String> {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [count(films, "film", "films"), count(shows, "show", "shows")]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| format!("In {} on this server", parts.join(" and ")))
}

#[component]
pub fn PersonView(page: PersonPage) -> Element {
    use_key_mode(Mode::Detail);
    let summary = appears_in(page.films.len(), page.shows.len());
    rsx! {
        article { class: "pharos-person",
            div { class: "pharos-person-photo",
                match &page.image_url {
                    Some(src) => rsx! { img { src: "{src}", alt: "", decoding: "async" } },
                    None => rsx! { div { class: "pharos-person-initials", aria_hidden: "true", "{initials(&page.name)}" } },
                }
            }
            div { class: "pharos-person-body",
                h2 { "{page.name}" }
                match summary {
                    Some(line) => rsx! { p { class: "pharos-muted", "{line}" } },
                    None => rsx! { p { class: "pharos-muted", "Nothing they act in is on this server." } },
                }
                ul { class: "pharos-person-links", aria_label: "Elsewhere",
                    li {
                        a { class: "pharos-button pharos-button-secondary",
                            href: "{imdb_url(&page.name)}", target: "_blank", rel: "noopener noreferrer",
                            "IMDb"
                        }
                    }
                    li {
                        a { class: "pharos-button pharos-button-secondary",
                            href: "{wikipedia_url(&page.name)}", target: "_blank", rel: "noopener noreferrer",
                            "Wikipedia"
                        }
                    }
                }
            }
        }
        if !page.films.is_empty() {
            section { class: "pharos-person-section", aria_label: "Films",
                h3 { "Films" }
                ul { class: "pharos-person-grid",
                    for film in page.films.iter().cloned() {
                        li { key: "{film.item.id}", FilmCredit { film } }
                    }
                }
            }
        }
        if !page.shows.is_empty() {
            section { class: "pharos-person-section", aria_label: "TV",
                h3 { "TV" }
                ul { class: "pharos-person-shows",
                    for show in page.shows.iter().cloned() {
                        li { key: "{show.series.id}", ShowCredit { show } }
                    }
                }
            }
        }
    }
}

#[component]
fn FilmCredit(film: Appearance) -> Element {
    rsx! {
        PosterTile { item: film.item }
        if !film.character.is_empty() {
            span { class: "pharos-person-character", title: "as {film.character}", "as {film.character}" }
        }
    }
}

#[component]
fn ShowCredit(show: ShowAppearances) -> Element {
    let count = show.episodes.len();
    let episodes = if count == 1 {
        "1 episode".to_owned()
    } else {
        format!("{count} episodes")
    };
    let playing = show.characters.join(", ");
    rsx! {
        div { class: "pharos-person-show",
            div { class: "pharos-person-show-poster", PosterTile { item: show.series.clone() } }
            div { class: "pharos-person-show-body",
                h4 { class: "pharos-person-show-name",
                    Link { to: open_route(&show.series), "{show.series.name}" }
                }
                if !playing.is_empty() {
                    p { class: "pharos-person-character", "as {playing}" }
                }
                details { class: "pharos-person-episodes", open: count <= OPEN_EPISODES,
                    summary { "{episodes}" }
                    ol {
                        for e in show.episodes.iter().cloned() {
                            li { key: "{e.item.id}", EpisodeCredit { episode: e, show_character: show.characters.len() > 1 } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn EpisodeCredit(episode: Appearance, show_character: bool) -> Element {
    let label = episode_label(&episode.item);
    let name = display_name(&episode.item);
    let guest = episode.kind == CreditKind::GuestStar;
    rsx! {
        Link { to: Route::Title(episode.item.id), class: "pharos-person-episode",
            if let Some(label) = label {
                span { class: "pharos-person-episode-number", "{label}" }
            }
            span { class: "pharos-person-episode-name", "{name}" }
            if episode.item.played {
                span { class: "pharos-muted", " · Watched" }
            }
            if guest {
                span { class: "pharos-person-guest", "Guest star" }
            }
            if show_character && !episode.character.is_empty() {
                span { class: "pharos-muted", " as {episode.character}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use pharos_client::api::{Item, ItemKind, ItemRef};

    fn render(page: PersonPage) -> String {
        use super::super::views::{MarkPlayed, Nav};
        let mut dom = VirtualDom::new_with_props(
            |page: PersonPage| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                use_context_provider(|| {
                    MarkPlayed(EventHandler::new(|_: (ItemRef, bool, String)| {}))
                });
                rsx! { PersonView { page } }
            },
            page,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn item(id: ItemRef, kind: ItemKind, name: &str) -> Item {
        Item {
            id,
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
        }
    }

    fn episode(id: u64, season: u32, number: u32, kind: CreditKind) -> Appearance {
        Appearance {
            item: Item {
                series_name: Some("Voyage".into()),
                season: Some(season),
                episode: Some(number),
                ..item(
                    ItemRef::Real(id),
                    ItemKind::Episode,
                    &format!("Part {number}"),
                )
            },
            character: "Captain".into(),
            kind,
        }
    }

    #[test]
    fn links_out_search_by_the_name_encoded() {
        assert_eq!(
            imdb_url("Zoë Saldaña"),
            "https://www.imdb.com/find/?q=Zo%C3%AB%20Salda%C3%B1a&s=nm"
        );
        assert_eq!(
            wikipedia_url("Sam & Max"),
            "https://en.wikipedia.org/w/index.php?search=Sam%20%26%20Max&title=Special%3ASearch&go=Go"
        );
    }

    #[test]
    fn the_summary_counts_films_and_shows() {
        assert_eq!(appears_in(0, 0), None);
        assert_eq!(
            appears_in(1, 0).as_deref(),
            Some("In 1 film on this server")
        );
        assert_eq!(
            appears_in(3, 1).as_deref(),
            Some("In 3 films and 1 show on this server")
        );
    }

    #[test]
    fn the_page_lists_films_then_shows_with_their_episodes() {
        let page = PersonPage {
            id: Some(ItemRef::Synth(9)),
            name: "Jane Lead".into(),
            image_url: Some("/Items/p/Images/Primary?tag=t".into()),
            films: vec![Appearance {
                item: item(ItemRef::Real(1), ItemKind::Movie, "Alien"),
                character: "Ripley".into(),
                kind: CreditKind::Actor,
            }],
            shows: vec![ShowAppearances {
                series: item(ItemRef::Synth(5), ItemKind::Series, "Voyage"),
                characters: vec!["Captain".into()],
                episodes: vec![
                    episode(10, 1, 1, CreditKind::Actor),
                    episode(11, 1, 2, CreditKind::GuestStar),
                ],
            }],
        };
        let html = render(page);
        let films = html.find(r#"aria-label="Films""#).expect("films");
        let tv = html.find(r#"aria-label="TV""#).expect("tv");
        assert!(films < tv, "{html}");
        assert!(
            html.contains("In 1 film and 1 show on this server"),
            "{html}"
        );
        assert!(html[films..tv].contains("as Ripley"), "{html}");
        assert!(html[films..tv].contains(r#"href="/ui/title/1""#), "{html}");
        assert!(
            html[tv..].contains(r#"href="/ui/browse/s5""#),
            "the show opens its listing: {html}"
        );
        assert!(
            html[tv..].contains("as Captain") && html[tv..].contains("2 episodes"),
            "{html}"
        );
        assert!(
            html[tv..].contains(r#"href="/ui/title/10""#) && html[tv..].contains("S1 · E2"),
            "{html}"
        );
        assert_eq!(html.matches("Guest star").count(), 1, "{html}");
        assert!(
            html.contains(r#"src="/Items/p/Images/Primary?tag=t""#),
            "{html}"
        );
        assert!(
            html.contains("https://www.imdb.com/find/?q=Jane%20Lead&#38;s=nm"),
            "{html}"
        );
        assert!(
            html.contains("https://en.wikipedia.org/w/index.php?search=Jane%20Lead"),
            "{html}"
        );
    }

    #[test]
    fn a_long_run_of_episodes_starts_folded() {
        let show = |n: u32| ShowAppearances {
            series: item(ItemRef::Synth(5), ItemKind::Series, "Voyage"),
            characters: vec!["Captain".into()],
            episodes: (1..=n)
                .map(|e| episode(u64::from(e), 1, e, CreditKind::Actor))
                .collect(),
        };
        let page = |n| PersonPage {
            name: "Jane Lead".into(),
            shows: vec![show(n)],
            ..PersonPage::default()
        };
        let details_tag = |html: &str| {
            let at = html.find("<details").expect("details");
            let end = html[at..].find('>').expect("tag end") + at;
            html[at..end].to_owned()
        };
        let short = render(page(OPEN_EPISODES as u32));
        assert!(details_tag(&short).contains("open=true"), "{short}");
        let long = render(page(OPEN_EPISODES as u32 + 1));
        assert!(!details_tag(&long).contains("open=true"), "{long}");
    }

    #[test]
    fn someone_in_nothing_here_says_so() {
        let html = render(PersonPage {
            name: "Jane Lead".into(),
            ..PersonPage::default()
        });
        assert!(
            html.contains("Nothing they act in is on this server."),
            "{html}"
        );
        assert!(
            html.contains(">JL<"),
            "initials stand in for a photo: {html}"
        );
    }
}
