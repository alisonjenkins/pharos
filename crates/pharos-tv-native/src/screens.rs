//! The spike's screens and fixture data, shared by the app and the host
//! snapshot example.

use dioxus::prelude::*;
use pharos_client::api::{Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind};
use pharos_ui::native::route::Route;
use pharos_ui::native::views::{Nav, PosterTile};

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

/// Posters per grid row at 960 CSS px wide (`minmax(9rem, 1fr)`, 1rem gap).
const GRID_COLUMNS: usize = 6;
/// Poster rows a rail shows at once.
const RAIL_VISIBLE: usize = 6;
/// One grid row's height in CSS px (2:3 art of ~147 px, name, year, gap).
const GRID_ROW_PX: usize = 290;
/// One home rail's height in CSS px.
const RAIL_ROW_PX: usize = 362;

/// Blitz 0.2 has no spatial navigation and dioxus-native 0.7 cannot read an
/// element's rect, so the spike moves a logical focus over the known layout
/// and shows it with a class. Measures what a press costs to re-render.
const SPIKE_CSS: &str = r#"
.pharos-tv-focus .pharos-poster-art {
  outline: 4px solid var(--px-color-accent);
  transform: scale(1.08);
}
.pharos-tv-viewport { height: 100vh; overflow: hidden; }
.pharos-tv-keysink { position: absolute; width: 1px; height: 1px; opacity: 0; }
"#;

#[component]
pub fn Spike(start: Screen) -> Element {
    let mut screen = use_signal(|| start);
    // (row, column) of the focused poster on the current screen.
    let mut focus = use_signal(|| (0usize, 0usize));
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            log::info!("nav {}", to.path());
        }))
    });
    let home = use_hook(fixture_home);
    let rails: Vec<(String, Vec<Item>)> = use_hook(|| {
        let mut rails = vec![
            ("Continue watching".to_owned(), home.resume.clone()),
            ("Next up".to_owned(), home.next_up.clone()),
        ];
        rails.extend(home.rails.iter().map(|r| (r.library.name.clone(), r.latest.clone())));
        rails
    });
    let grid: Vec<Item> = use_hook(|| {
        (1..=120)
            .map(|n| item(n, ItemKind::Movie, format!("Poster {n}")))
            .collect()
    });
    let mut last_key = use_signal(|| None::<std::time::Instant>);
    use_effect(move || {
        // Runs after the render that follows a key: a rough press-to-render
        // time (the frame itself is committed after this).
        let _ = focus();
        let _ = screen();
        if let Some(at) = *last_key.peek() {
            log::info!("bench render_after_key_us={}", at.elapsed().as_micros());
        }
    });
    let theme = use_hook(theme_css);
    let (rows, cols_of): (usize, Box<dyn Fn(usize) -> usize>) = match screen() {
        Screen::Home => {
            let lens: Vec<usize> = rails.iter().map(|r| r.1.len()).collect();
            (lens.len(), Box::new(move |r| lens[r]))
        }
        Screen::Grid => {
            let n = grid.len();
            (n.div_ceil(GRID_COLUMNS), Box::new(move |r| (n - r * GRID_COLUMNS).min(GRID_COLUMNS)))
        }
    };
    let (frow, fcol) = focus();
    let home_y = frow.saturating_sub(1) * RAIL_ROW_PX;
    let grid_y = frow.saturating_sub(1) * GRID_ROW_PX;
    let shifts: Vec<usize> = (0..rails.len())
        .map(|r| if r == frow { (fcol + 1).saturating_sub(RAIL_VISIBLE) * 160 } else { 0 })
        .collect();
    rsx! {
        style { {theme} }
        style { {CSS} }
        style { {SPIKE_CSS} }
        div {
            class: "pharos-app pharos-tv-viewport",
            onkeydown: move |e: KeyboardEvent| {
                let key = e.key();
                last_key.set(Some(std::time::Instant::now()));
                let (r, c) = focus();
                let next = match key {
                    Key::ArrowUp => (r.saturating_sub(1), c),
                    Key::ArrowDown => ((r + 1).min(rows - 1), c),
                    Key::ArrowLeft => (r, c.saturating_sub(1)),
                    Key::ArrowRight => (r, c + 1),
                    Key::Enter => {
                        log::info!("open row {r} col {c}");
                        (r, c)
                    }
                    Key::BrowserBack | Key::GoBack if screen() == Screen::Grid => {
                        screen.set(Screen::Home);
                        (0, 0)
                    }
                    Key::MediaPlayPause | Key::Character(_) => {
                        screen.set(if screen() == Screen::Home { Screen::Grid } else { Screen::Home });
                        (0, 0)
                    }
                    _ => (r, c),
                };
                let next = (next.0, next.1.min(cols_of(next.0).saturating_sub(1)));
                log::info!("key {key:?} focus {next:?}");
                focus.set(next);
            },
            // Blitz 0.2 works out `is_focussable` when an element is created,
            // before Dioxus sets `tabindex`, so a `div` never takes focus. A
            // `button` is focussable by tag; keys bubble from it to the root.
            button { class: "pharos-tv-keysink", autofocus: true }
            match screen() {
                Screen::Home => rsx! {
                    div { style: "transform: translateY(-{home_y}px)",
                        for (r, ((title, items), shift)) in rails.iter().zip(shifts.iter()).enumerate() {
                            section { class: "pharos-rail",
                                h2 { class: "pharos-rail-title", "{title}" }
                                ul { class: "pharos-rail-items",
                                    style: "display: flex; gap: 1rem; transform: translateX(-{shift}px)",
                                    for (c, it) in items.iter().cloned().enumerate() {
                                        li { key: "{it.id}",
                                            class: if r == frow && c == fcol { "pharos-tv-focus" },
                                            PosterTile { item: it }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                Screen::Grid => rsx! {
                    ul { class: "pharos-grid",
                        style: "transform: translateY(-{grid_y}px)",
                        for (i, it) in grid.iter().cloned().enumerate() {
                            li { key: "{it.id}",
                                class: if i == frow * GRID_COLUMNS + fcol { "pharos-tv-focus" },
                                PosterTile { item: it }
                            }
                        }
                    }
                },
            }
        }
    }
}
