//! The spike's screens and fixture data, shared by the app and the host
//! snapshot example.

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind};
use pharos_ui::native::route::Route;
use pharos_ui::native::views::{MarkPlayed, Nav, PosterTile, TitleView};

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
/// One grid row's height in CSS px (254 px tile and a 16 px gap).
const GRID_ROW_PX: usize = 270;
/// One grid tile's height in CSS px, kept by rows drawn as empty boxes.
const GRID_TILE_PX: usize = GRID_ROW_PX - 16;
/// Grid rows above and below the focused one that get real tiles. Blitz has
/// no `loading="lazy"`, so every `<img>` in the tree is fetched and decoded;
/// rows outside this window are empty boxes of the same height.
const GRID_ROWS_BEHIND: usize = 1;
const GRID_ROWS_AHEAD: usize = 3;

fn near(row: usize, focused: usize) -> bool {
    row + GRID_ROWS_BEHIND >= focused && row <= focused + GRID_ROWS_AHEAD
}

/// One home rail's height in CSS px.
const RAIL_ROW_PX: usize = 362;

/// Blitz 0.2 has no spatial navigation and dioxus-native 0.7 cannot read an
/// element's rect, so the spike moves a logical focus over the known layout
/// and shows it with a class. Measures what a press costs to re-render.
/// Rows move by negative margins, not transforms: blitz-paint 0.2 does not
/// apply a parent's transform to its children ("TODO: Implement nested
/// transforms" in render.rs).
const SPIKE_CSS: &str = r#"
.pharos-tv-focus .pharos-poster-art {
  outline: 4px solid var(--px-color-accent);
  transform: scale(1.08);
}
.pharos-tv-viewport { height: 100vh; overflow: hidden; }
.pharos-tv-notice {
  position: absolute; top: 2rem; right: 2rem; z-index: 10; margin: 0;
  padding: 1rem 1.5rem; border-radius: 0.5rem; font-size: 2rem;
  background: var(--px-color-surface, #222); color: var(--px-color-text, #fff);
}
.pharos-tv-keysink { position: absolute; width: 1px; height: 1px; opacity: 0; }
"#;

const LITE_POSTERS: &str = r#"
.pharos-poster-art { box-shadow: none; }
.pharos-poster-art::before, .pharos-poster-art::after { display: none; }
"#;
const LITE_BACKDROP: &str = "body::before { display: none; }";

/// What the screens show: rails on home and the library grid.
#[derive(Clone, PartialEq, Debug)]
pub struct Data {
    pub rails: Vec<(String, Vec<Item>)>,
    pub grid: Vec<Item>,
}

impl Data {
    /// Home's rails in the WebView app's order, and a library page.
    pub fn new(home: &Home, grid: Vec<Item>) -> Self {
        let mut rails = vec![
            ("Continue watching".to_owned(), home.resume.clone()),
            ("Next up".to_owned(), home.next_up.clone()),
        ];
        rails.extend(home.rails.iter().map(|r| (r.library.name.clone(), r.latest.clone())));
        rails.retain(|(_, items)| !items.is_empty());
        Self { rails, grid }
    }

    pub fn fixture() -> Self {
        let grid = (1..=120)
            .map(|n| item(n, ItemKind::Movie, format!("Poster {n}")))
            .collect();
        Self::new(&fixture_home(), grid)
    }
}

/// `notice` is shown over the screens: the QuickConnect code, or why the
/// live server could not be reached. OK on a poster calls `on_open`; while
/// `detail` is set its item page shows, and Back calls `on_back`.
#[component]
pub fn Spike(
    start: Screen,
    data: Data,
    #[props(default)] notice: Option<String>,
    #[props(default)] detail: Option<Detail>,
    #[props(default)] on_open: EventHandler<ItemRef>,
    #[props(default)] on_back: EventHandler<()>,
) -> Element {
    let mut screen = use_signal(|| start);
    // (row, column) of the focused poster on the current screen.
    let mut focus = use_signal(|| (0usize, 0usize));
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            log::info!("nav {}", to.path());
        }))
    });
    // The item page's watched toggle; the spike logs it and stores nothing.
    use_context_provider(|| {
        MarkPlayed(EventHandler::new(move |(id, played): (ItemRef, bool)| {
            log::info!("mark played {id} {played}");
        }))
    });
    let has_detail = detail.is_some();
    let Data { rails, grid } = data;
    let rail_ids: Vec<Vec<ItemRef>> =
        rails.iter().map(|(_, items)| items.iter().map(|i| i.id).collect()).collect();
    let grid_ids: Vec<ItemRef> = grid.iter().map(|i| i.id).collect();
    let mut last_key = use_signal(|| None::<std::time::Instant>);
    // Paint-cost probe: "s" steps through 0 (shipped CSS), 1 (no poster
    // shadows or overlays) and 2 (also no full-screen backdrop gradient).
    let mut lite = use_signal(|| 0u8);
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
        if let Some(notice) = notice {
            p { class: "pharos-tv-notice", "{notice}" }
        }
        if lite() >= 1 { style { {LITE_POSTERS} } }
        if lite() >= 2 { style { {LITE_BACKDROP} } }
        div {
            class: "pharos-app pharos-tv-viewport",
            onkeydown: move |e: KeyboardEvent| {
                let key = e.key();
                last_key.set(Some(std::time::Instant::now()));
                let (r, c) = focus();
                let next = match key {
                    Key::Named(NamedKey::ArrowUp) => (r.saturating_sub(1), c),
                    Key::Named(NamedKey::ArrowDown) => ((r + 1).min(rows.saturating_sub(1)), c),
                    Key::Named(NamedKey::ArrowLeft) => (r, c.saturating_sub(1)),
                    Key::Named(NamedKey::ArrowRight) => (r, c + 1),
                    Key::Named(NamedKey::BrowserBack) | Key::Named(NamedKey::GoBack) if has_detail => {
                        on_back.call(());
                        (r, c)
                    }
                    _ if has_detail => (r, c),
                    Key::Named(NamedKey::Enter) => {
                        let picked = match screen() {
                            Screen::Home => rail_ids.get(r).and_then(|ids| ids.get(c)),
                            Screen::Grid => grid_ids.get(r * GRID_COLUMNS + c),
                        };
                        log::info!("open row {r} col {c}");
                        if let Some(id) = picked {
                            on_open.call(*id);
                        }
                        (r, c)
                    }
                    Key::Named(NamedKey::BrowserBack) | Key::Named(NamedKey::GoBack) if screen() == Screen::Grid => {
                        screen.set(Screen::Home);
                        (0, 0)
                    }
                    Key::Character(ref ch) if ch == "s" => {
                        let level = (lite() + 1) % 3;
                        log::info!("lite level {level}");
                        lite.set(level);
                        (r, c)
                    }
                    Key::Named(NamedKey::MediaPlayPause) | Key::Character(_) => {
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
            if let Some(detail) = detail {
                TitleView { detail, tracks: None }
            } else {
            match screen() {
                Screen::Home => rsx! {
                    div { style: "margin-top: -{home_y}px",
                        for (r, ((title, items), shift)) in rails.iter().zip(shifts.iter()).enumerate() {
                            section { class: "pharos-rail",
                                h2 { class: "pharos-rail-title", "{title}" }
                                ul { class: "pharos-rail-items",
                                    style: "display: flex; gap: 1rem; margin-left: -{shift}px",
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
                        style: "margin-top: -{grid_y}px",
                        for (i, it) in grid.iter().cloned().enumerate() {
                            li { key: "{it.id}",
                                class: if i == frow * GRID_COLUMNS + fcol { "pharos-tv-focus" },
                                if near(i / GRID_COLUMNS, frow) {
                                    PosterTile { item: it }
                                } else {
                                    div { style: "height: {GRID_TILE_PX}px" }
                                }
                            }
                        }
                    }
                },
            }
            }
        }
    }
}
