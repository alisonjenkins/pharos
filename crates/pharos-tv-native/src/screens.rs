//! The spike's screens and fixture data, shared by the app and the host
//! snapshot example.

use dioxus::prelude::*;
use pharos_client::api::{Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind};
use pharos_ui::native::route::Route;
use pharos_ui::native::views::{HomeView, Nav, PosterTile};

pub const CSS: &str = include_str!("../../pharos-ui/assets/pharos.css");

/// The default theme's custom properties (`--px-*`). The web app injects
/// them at runtime; without them every themed colour, radius and font is
/// unset.
pub fn theme_css() -> String {
    use pharos_ui::theme;
    theme::shipped()
        .resolve(theme::DEFAULT)
        .map(|resolved| theme::css::emit(&resolved.theme))
        .unwrap_or_default()
}

pub fn item(n: u64, kind: ItemKind, name: String) -> Item {
    Item {
        id: ItemRef::Real(n),
        kind,
        name,
        year: Some(1990 + (n % 35) as u16),
        runtime_ms: 5_400_000,
        image: None,
        played: n % 7 == 0,
        favorite: false,
        position_ms: if n % 5 == 0 { 1_800_000 } else { 0 },
        series_name: None,
        season: None,
        episode: None,
        parent: None,
        library_kind: None,
        imdb_rating: Some(60 + (n % 30) as u16),
        critic_rating: None,
        artist: None,
    }
}

fn fixture_home() -> Home {
    let library = |n: u64, name: &str, kind: LibraryKind| Item {
        library_kind: Some(kind),
        ..item(n, ItemKind::Library, name.to_owned())
    };
    let films: Vec<Item> = (1..=16)
        .map(|n| item(n, ItemKind::Movie, format!("Film {n}")))
        .collect();
    Home {
        rails: vec![
            HomeRail { library: library(900, "Films", LibraryKind::Movies), latest: films.clone() },
            HomeRail { library: library(901, "Shows", LibraryKind::TvShows), latest: films.clone() },
        ],
        resume: films[..6].to_vec(),
        listening: vec![],
        next_up: films[6..12].to_vec(),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Screen {
    Home,
    Grid,
}

#[component]
pub fn Spike(start: Screen) -> Element {
    let mut screen = use_signal(|| start);
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            log::info!("nav {}", to.path());
        }))
    });
    let home = use_hook(fixture_home);
    let grid: Vec<Item> = use_hook(|| {
        (1..=120)
            .map(|n| item(n, ItemKind::Movie, format!("Poster {n}")))
            .collect()
    });
    let mut last_key = use_signal(|| None::<std::time::Instant>);
    use_effect(move || {
        // Runs after the render that follows a key: a rough press-to-render
        // time (the frame itself is committed after this).
        if let Some(at) = *last_key.read() {
            log::info!("bench render_after_key_us={}", at.elapsed().as_micros());
        }
    });
    let theme = use_hook(theme_css);
    rsx! {
        style { {theme} }
        style { {CSS} }
        div {
            class: "pharos-app",
            onkeydown: move |e: KeyboardEvent| {
                log::info!("key {:?}", e.key());
                last_key.set(Some(std::time::Instant::now()));
                if e.key() == Key::Character("g".into()) || e.key() == Key::MediaPlayPause {
                    screen.set(if screen() == Screen::Home { Screen::Grid } else { Screen::Home });
                }
            },
            match screen() {
                Screen::Home => rsx! { HomeView { home: home.clone() } },
                Screen::Grid => rsx! {
                    ul { class: "pharos-grid",
                        for it in grid.iter().cloned() {
                            li { key: "{it.id}", PosterTile { item: it } }
                        }
                    }
                },
            }
        }
    }
}
