//! Spec 032 slice 0 spike: the shared components drawn by Dioxus Native
//! (Blitz) on Android TV, no WebView. Renders the home rails, a 120-poster
//! grid and logs every key with its timing under the `pharos-bench` tag.
//!
//! Fixture data only; the live server comes with slice 1.

use dioxus::prelude::*;
use pharos_client::api::{Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind};
use pharos_ui::native::route::Route;
use pharos_ui::native::views::{HomeView, Nav, PosterTile};

const CSS: &str = include_str!("../../pharos-ui/assets/pharos.css");

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("pharos-bench"),
    );
    blitz_shell::set_android_app(app);
    log::info!("pharos-tv-native start");
    main();
}

fn main() {
    dioxus::launch(root);
}

fn item(n: u64, kind: ItemKind, name: String) -> Item {
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

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Home,
    Grid,
}

fn root() -> Element {
    let mut screen = use_signal(|| Screen::Home);
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
    rsx! {
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
