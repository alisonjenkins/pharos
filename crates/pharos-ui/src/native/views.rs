//! The native UI's screens as pure components: data in through props,
//! navigation out through the [`Nav`] context. No IO here, so every screen
//! renders under `dioxus-ssr` in tests (009 FR-014).

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, HomeRail, Item, ItemKind, LibraryKind, Page};

use super::depth;
use super::route::Route;

/// Navigation, provided by the app root: a click on a [`Link`] calls it
/// instead of reloading the page.
#[derive(Clone, Copy)]
pub struct Nav(pub EventHandler<Route>);

/// An `<a>` to a route. A plain click navigates in place; a modified click
/// (new tab, new window) is left to the browser.
#[component]
pub fn Link(to: Route, class: Option<String>, children: Element) -> Element {
    let nav = use_context::<Nav>();
    let href = to.path();
    rsx! {
        a {
            href: "{href}",
            class,
            onclick: move |evt: MouseEvent| {
                if evt.modifiers().is_empty() {
                    evt.prevent_default();
                    nav.0.call(to.clone());
                }
            },
            {children}
        }
    }
}

/// The frame around every signed-in screen.
#[component]
pub fn Shell(on_sign_out: EventHandler<()>, children: Element) -> Element {
    rsx! {
        div { class: "pharos-app",
            header { class: "pharos-banner pharos-shell-bar",
                Link { to: Route::Home, class: "pharos-brand", h1 { "pharos" } }
                nav { class: "pharos-shell-nav",
                    Link { to: Route::Find, "Find" }
                    Link { to: Route::Appearance, "Appearance" }
                    Link { to: Route::Manage, "Manage" }
                    button {
                        class: "pharos-link-button",
                        onclick: move |_| on_sign_out.call(()),
                        "Sign out"
                    }
                }
            }
            main { class: "pharos-main", {children} }
        }
    }
}

#[component]
pub fn SignInView(
    error: Option<String>,
    busy: bool,
    on_submit: EventHandler<(String, String)>,
) -> Element {
    let mut name = use_signal(String::new);
    let mut password = use_signal(String::new);
    rsx! {
        div { class: "pharos-app",
            header { class: "pharos-banner", h1 { "pharos" } }
            form {
                class: "pharos-login",
                id: "signin",
                onsubmit: move |evt: FormEvent| {
                    evt.prevent_default();
                    on_submit.call((name.read().clone(), password.read().clone()));
                },
                label {
                    "Name"
                    input {
                        id: "signin-name",
                        name: "name",
                        autocomplete: "username",
                        autocapitalize: "none",
                        autocorrect: "off",
                        spellcheck: "false",
                        required: true,
                        value: "{name}",
                        oninput: move |e| name.set(e.value()),
                    }
                }
                label {
                    "Password"
                    input {
                        id: "signin-password",
                        name: "password",
                        r#type: "password",
                        autocomplete: "current-password",
                        value: "{password}",
                        oninput: move |e| password.set(e.value()),
                    }
                }
                if let Some(error) = error {
                    p { class: "pharos-error", role: "alert", "{error}" }
                }
                button { r#type: "submit", disabled: busy,
                    if busy { "Signing in…" } else { "Sign in" }
                }
            }
        }
    }
}

/// A full-page state with no data to show: connecting, not enabled, an
/// error. `action` adds a retry button.
#[component]
pub fn StatusView(
    title: String,
    detail: String,
    action: Option<(String, EventHandler<()>)>,
) -> Element {
    rsx! {
        div { class: "pharos-app",
            header { class: "pharos-banner", h1 { "pharos" } }
            section { class: "pharos-status", role: "status",
                h2 { "{title}" }
                p { "{detail}" }
                if let Some((label, on_click)) = action {
                    button { onclick: move |_| on_click.call(()), "{label}" }
                }
            }
        }
    }
}

#[component]
pub fn Loading() -> Element {
    rsx! { p { class: "pharos-loading", role: "status", "Loading…" } }
}

#[component]
pub fn ErrorNote(message: String) -> Element {
    rsx! { p { class: "pharos-error", role: "alert", "{message}" } }
}

#[component]
pub fn HomeView(home: Home) -> Element {
    let empty = home.rails.is_empty()
        && home.resume.is_empty()
        && home.listening.is_empty()
        && home.next_up.is_empty();
    let mut rails: Vec<&HomeRail> = home.rails.iter().collect();
    rails.sort_by_key(|rail| rail_rank(rail.library.library_kind));
    // Music to resume sits just before the music libraries.
    let music = rail_rank(Some(LibraryKind::Music));
    let split = rails.partition_point(|rail| rail_rank(rail.library.library_kind) < music);
    let (before_music, from_music) = rails.split_at(split);
    rsx! {
        div { class: "pharos-home",
            if empty {
                p { class: "pharos-empty", "Nothing here yet. Add a library from Manage." }
            }
            if !home.resume.is_empty() {
                Rail { title: "Continue watching", items: home.resume.clone() }
            }
            if !home.next_up.is_empty() {
                Rail { title: "Next up", items: home.next_up.clone() }
            }
            for rail in before_music {
                LibraryRail { key: "{rail.library.id}", rail: (*rail).clone() }
            }
            if !home.listening.is_empty() {
                Rail { title: "Continue listening", items: home.listening.clone() }
            }
            for rail in from_music {
                LibraryRail { key: "{rail.library.id}", rail: (*rail).clone() }
            }
        }
    }
}

/// A library's newest items, titled with a link to the whole library.
#[component]
fn LibraryRail(rail: HomeRail) -> Element {
    rsx! {
        Rail {
            title: rail.library.name.clone(),
            link: Route::Browse(rail.library.id),
            items: rail.latest.clone(),
        }
    }
}

/// Where a library's rail sits on the home page: shows, films, music, books,
/// then mixed libraries. The server's order holds within one kind.
fn rail_rank(kind: Option<LibraryKind>) -> u8 {
    match kind {
        Some(LibraryKind::TvShows) => 0,
        Some(LibraryKind::Movies) => 1,
        Some(LibraryKind::Music) => 2,
        Some(LibraryKind::Books) => 3,
        Some(LibraryKind::Mixed) => 4,
        None => 5,
    }
}

/// A titled row of posters; `link` makes the title open the full listing.
#[component]
pub fn Rail(title: String, link: Option<Route>, items: Vec<Item>) -> Element {
    rsx! {
        section { class: "pharos-rail",
            h2 { class: "pharos-rail-title",
                match link {
                    Some(to) => rsx! { Link { to, "{title}" } },
                    None => rsx! { "{title}" },
                }
            }
            ul { class: "pharos-rail-items",
                for item in items {
                    li { key: "{item.id}", PosterTile { item: item.clone() } }
                }
            }
        }
    }
}

/// What the find screen shows under its box.
#[derive(Debug, Clone, PartialEq)]
pub enum Hits {
    /// Nothing asked yet: the box is blank.
    Idle,
    /// The matches for the newest term; empty when nothing matched.
    Found(Page),
    Failed(String),
}

const FIND_INPUT: &str = "find-input";

/// The search box and the matches under it. Typing reports each new text
/// through `on_input`; the grid is the same poster tiles a library shows.
#[component]
pub fn FindView(text: String, hits: Hits, on_input: EventHandler<String>) -> Element {
    rsx! {
        section { class: "pharos-find",
            h2 { "Find" }
            div { class: "pharos-find-field",
                label { r#for: FIND_INPUT, "Search" }
                input {
                    id: FIND_INPUT,
                    r#type: "search",
                    autocomplete: "off",
                    value: "{text}",
                    oninput: move |evt: FormEvent| on_input.call(evt.value()),
                }
            }
            match hits {
                Hits::Idle => rsx! {},
                Hits::Failed(message) => rsx! { ErrorNote { message } },
                Hits::Found(page) if page.items.is_empty() => rsx! {
                    p { class: "pharos-empty", role: "status", "No matches" }
                },
                Hits::Found(page) => rsx! {
                    p { class: "pharos-muted", role: "status", "{page.total} found" }
                    ul { class: "pharos-grid",
                        for item in page.items.iter() {
                            li { key: "{item.id}", PosterTile { item: item.clone() } }
                        }
                    }
                },
            }
        }
    }
}

/// Where a poster leads: a container opens its listing, anything else its
/// title page.
pub fn open_route(item: &Item) -> Route {
    match item.kind {
        ItemKind::Library
        | ItemKind::Series
        | ItemKind::Season
        | ItemKind::Album
        | ItemKind::Artist => Route::Browse(item.id),
        _ => Route::Title(item.id),
    }
}

#[component]
pub fn PosterTile(item: Item) -> Element {
    let sub = sub_lines(&item);
    let progress = progress_percent(&item);
    rsx! {
        Link { to: open_route(&item), class: "pharos-poster",
            div { class: "pharos-poster-art",
                onpointermove: move |evt| depth::tilt(&evt),
                onpointerleave: move |evt| depth::untilt(&evt),
                match &item.image {
                    Some(src) => rsx! { img { src: "{src}", alt: "", loading: "lazy", decoding: "async" } },
                    None => rsx! { ArtPlaceholder { kind: item.kind, name: item.name.clone() } },
                }
                if item.played {
                    span { class: "pharos-badge", "Watched" }
                }
                if let Some(p) = progress {
                    div { class: "pharos-progress", style: "width: {p}%" }
                }
            }
            span { class: "pharos-poster-name", title: "{item.name}", "{item.name}" }
            for line in sub {
                span { class: "pharos-poster-sub", title: "{line}", "{line}" }
            }
        }
    }
}

#[component]
pub fn TitleView(detail: Detail) -> Element {
    let item = &detail.item;
    let facts: Vec<String> = [
        item.year.map(|y| y.to_string()),
        runtime(item.runtime_ms),
        index_label(item),
    ]
    .into_iter()
    .flatten()
    .collect();
    let facts = facts.join(" · ");
    let resume = progress_percent(item).is_some();
    let track = item.kind == ItemKind::Audio;
    // A track's byline is its artist, where an episode's is its series.
    let byline = if track {
        &item.artist
    } else {
        &item.series_name
    };
    let album = item.album.as_ref().filter(|_| track);
    rsx! {
        article { class: "pharos-title",
            div { class: if track { "pharos-title-art pharos-title-art-square" } else { "pharos-title-art" },
                match &item.image {
                    Some(src) => rsx! { img { src: "{src}", alt: "" } },
                    None => rsx! { ArtPlaceholder { kind: item.kind, name: item.name.clone() } },
                }
            }
            div { class: "pharos-title-body",
                if let Some(byline) = byline {
                    p { class: "pharos-muted pharos-title-byline", "{byline}" }
                }
                h2 { "{item.name}" }
                if let Some(album) = album {
                    p { class: "pharos-title-album",
                        match item.parent {
                            Some(parent) => rsx! { Link { to: Route::Browse(parent), "{album}" } },
                            None => rsx! { "{album}" },
                        }
                    }
                }
                if !facts.is_empty() {
                    p { class: "pharos-muted", "{facts}" }
                }
                div { class: "pharos-actions",
                    if item.kind.playable() {
                        Link { to: Route::Watch(item.id), class: "pharos-button",
                            if resume { "Resume" } else { "Play" }
                        }
                    }
                    if matches!(open_route(item), Route::Browse(_)) {
                        Link { to: Route::Browse(item.id), class: "pharos-button", "Open" }
                    }
                }
                if !detail.overview.is_empty() {
                    p { class: "pharos-overview", "{detail.overview}" }
                }
                if !detail.genres.is_empty() {
                    p { class: "pharos-muted", "{detail.genres.join(\", \")}" }
                }
            }
        }
    }
}

/// What stands in for missing art: a tile in the theme's accent with a note
/// for music and the name's first letter for anything else (B320).
#[component]
pub fn ArtPlaceholder(kind: ItemKind, name: String) -> Element {
    let glyph = match kind {
        ItemKind::Audio | ItemKind::Album | ItemKind::Artist => "♪".to_owned(),
        _ => initial(&name),
    };
    rsx! {
        div { class: "pharos-poster-placeholder pharos-art-placeholder", aria_hidden: "true", "{glyph}" }
    }
}

fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

/// "1h 42m", "48m"; nothing for an item with no runtime (a book, a folder).
pub fn runtime(ms: u64) -> Option<String> {
    let minutes = ms / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, 0) => None,
        (0, m) => Some(format!("{m}m")),
        (h, 0) => Some(format!("{h}h")),
        (h, m) => Some(format!("{h}h {m}m")),
    }
}

/// "S1 · E3" for an episode with its numbers; "Track 3", or "Disc 2 ·
/// Track 3" past the first disc, for a song.
pub fn index_label(item: &Item) -> Option<String> {
    match (item.kind, item.season, item.episode) {
        (ItemKind::Episode, Some(s), Some(e)) => Some(format!("S{s} · E{e}")),
        (ItemKind::Episode, None, Some(e)) => Some(format!("E{e}")),
        (ItemKind::Audio, Some(d), Some(t)) if d > 1 => Some(format!("Disc {d} · Track {t}")),
        (ItemKind::Audio, _, Some(t)) => Some(format!("Track {t}")),
        _ => None,
    }
}

/// The lines under a poster's name: an episode's series, then its numbers,
/// each on its own line so a long series name cannot hide the numbers. A
/// song shows its artist, or its year when untagged.
fn sub_lines(item: &Item) -> Vec<String> {
    match item.kind {
        ItemKind::Episode => [item.series_name.clone(), index_label(item)]
            .into_iter()
            .flatten()
            .collect(),
        ItemKind::Audio if item.artist.is_some() => item.artist.clone().into_iter().collect(),
        _ => item.year.map(|y| y.to_string()).into_iter().collect(),
    }
}

/// How far into an unfinished item the user is, 1–99.
pub fn progress_percent(item: &Item) -> Option<u64> {
    if item.played || item.position_ms == 0 || item.runtime_ms == 0 {
        return None;
    }
    Some((item.position_ms * 100 / item.runtime_ms).clamp(1, 99))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_client::api::ItemRef;

    fn item(kind: ItemKind) -> Item {
        Item {
            id: ItemRef::Real(1),
            kind,
            name: "Name".into(),
            year: Some(2001),
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
            album: None,
            artist: None,
        }
    }

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom = VirtualDom::new_with_props(
            |body: std::rc::Rc<dyn Fn() -> Element>| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                body()
            },
            body,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn sign_in_shows_the_form_and_any_error() {
        let html = render(|| rsx! { SignInView { error: None, busy: false, on_submit: |_| {} } });
        assert!(html.contains(r#"autocomplete="username""#), "{html}");
        assert!(html.contains(r#"type="password""#), "{html}");
        assert!(!html.contains("pharos-error"), "{html}");
        let html = render(|| {
            rsx! { SignInView { error: Some("Wrong name or password.".into()), busy: true, on_submit: |_| {} } }
        });
        assert!(html.contains("Wrong name or password."), "{html}");
        assert!(html.contains("Signing in…"), "{html}");
    }

    /// A phone keyboard would capitalise or "correct" the name, and the
    /// server then would not find the account.
    #[test]
    fn the_sign_in_name_is_left_as_typed() {
        let html = render(|| rsx! { SignInView { error: None, busy: false, on_submit: |_| {} } });
        let input = html
            .split("<input")
            .find(|i| i.contains(r#"id="signin-name""#))
            .unwrap_or_else(|| panic!("no name input in {html}"));
        for want in [
            r#"autocomplete="username""#,
            r#"autocapitalize="none""#,
            r#"autocorrect="off""#,
            r#"spellcheck="false""#,
        ] {
            assert!(input.contains(want), "missing {want} in {input}");
        }
    }

    #[test]
    fn a_status_page_says_what_is_wrong() {
        let html = render(|| {
            rsx! { StatusView { title: "Not enabled yet".to_string(), detail: "Turn it on.".to_string(), action: None } }
        });
        assert!(
            html.contains("Not enabled yet") && html.contains("Turn it on."),
            "{html}"
        );
        assert!(!html.contains("<button"), "{html}");
    }

    #[test]
    fn home_shows_each_rail_with_its_library_link() {
        let mut lib = item(ItemKind::Library);
        lib.id = ItemRef::Real(7);
        lib.name = "Films".into();
        let mut movie = item(ItemKind::Movie);
        movie.id = ItemRef::Real(42);
        movie.name = "Heat".into();
        movie.runtime_ms = 1000;
        movie.position_ms = 250;
        let home = Home {
            rails: vec![pharos_client::api::HomeRail {
                library: lib,
                latest: vec![movie.clone()],
            }],
            resume: vec![movie],
            listening: vec![],
            next_up: vec![],
        };
        let html = render(move || rsx! { HomeView { home: home.clone() } });
        assert!(html.contains("Continue watching"), "{html}");
        assert!(!html.contains("Continue listening"), "{html}");
        assert!(!html.contains("Next up"), "{html}");
        assert!(
            html.contains(r#"href="/ui/browse/7""#) && html.contains("Films"),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/ui/title/42""#) && html.contains("Heat"),
            "{html}"
        );
        assert!(html.contains("width: 25%"), "{html}");
    }

    #[test]
    fn home_rails_run_shows_films_listening_music_books_then_mixed() {
        let rail = |id: u64, name: &str, kind: Option<LibraryKind>| {
            let mut lib = item(ItemKind::Library);
            lib.id = ItemRef::Real(id);
            lib.name = name.into();
            lib.library_kind = kind;
            HomeRail {
                library: lib,
                latest: vec![],
            }
        };
        let mut next = item(ItemKind::Episode);
        next.id = ItemRef::Real(99);
        let mut song = item(ItemKind::Audio);
        song.id = ItemRef::Real(98);
        let home = Home {
            rails: vec![
                rail(1, "Media", Some(LibraryKind::Mixed)),
                rail(2, "Books", Some(LibraryKind::Books)),
                rail(3, "Music", Some(LibraryKind::Music)),
                rail(4, "Movies", Some(LibraryKind::Movies)),
                rail(5, "TV Shows", Some(LibraryKind::TvShows)),
            ],
            resume: vec![item(ItemKind::Movie)],
            listening: vec![song],
            next_up: vec![next],
        };
        let html = render(move || rsx! { HomeView { home: home.clone() } });
        let at = |title: &str| {
            html.find(&format!(">{title}<"))
                .unwrap_or_else(|| panic!("no {title} in {html}"))
        };
        let order = [
            "Continue watching",
            "Next up",
            "TV Shows",
            "Movies",
            "Continue listening",
            "Music",
            "Books",
            "Media",
        ];
        for pair in order.windows(2) {
            assert!(at(pair[0]) < at(pair[1]), "{} before {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn music_resumes_under_continue_listening_not_watching() {
        let mut song = item(ItemKind::Audio);
        song.id = ItemRef::Real(9);
        song.name = "Carefree".into();
        let home = Home {
            listening: vec![song],
            ..Home::default()
        };
        let html = render(move || rsx! { HomeView { home: home.clone() } });
        assert!(html.contains("Continue listening"), "{html}");
        assert!(html.contains("Carefree"), "{html}");
        assert!(!html.contains("Continue watching"), "{html}");
        assert!(!html.contains("Add a library from Manage"), "{html}");
    }

    #[test]
    fn an_empty_home_points_at_manage() {
        let home = Home::default();
        let html = render(move || rsx! { HomeView { home: home.clone() } });
        assert!(html.contains("Add a library from Manage"), "{html}");
    }

    #[test]
    fn browse_lists_items_and_says_when_there_are_more() {
        let page = Page {
            items: vec![item(ItemKind::Series)],
            total: 3,
            start: 0,
        };
        let html = render(
            move || rsx! { crate::native::browse::GridView { title: "Shows".to_string(), page: page.clone(), library: false } },
        );
        assert!(html.contains(r#"href="/ui/browse/1""#), "{html}");
        assert!(html.contains("Showing 1 of 3."), "{html}");
    }

    #[test]
    fn a_title_offers_play_or_resume() {
        let mut movie = item(ItemKind::Movie);
        movie.runtime_ms = 102 * 60_000;
        let detail = Detail {
            item: movie.clone(),
            overview: "A heist.".into(),
            genres: vec!["Crime".into()],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(
            html.contains(r#"href="/ui/watch/1""#) && html.contains(">Play<"),
            "{html}"
        );
        assert!(
            html.contains("2001 · 1h 42m") && html.contains("A heist.") && html.contains("Crime"),
            "{html}"
        );

        movie.position_ms = 60_000;
        let detail = Detail {
            item: movie,
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(html.contains(">Resume<"), "{html}");
    }

    #[test]
    fn a_series_title_opens_its_listing_instead_of_playing() {
        let detail = Detail {
            item: item(ItemKind::Series),
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(!html.contains("/watch/"), "{html}");
        assert!(html.contains(r#"href="/ui/browse/1""#), "{html}");
    }

    #[test]
    fn runtimes_read_as_hours_and_minutes() {
        assert_eq!(runtime(0), None);
        assert_eq!(runtime(59_999), None);
        assert_eq!(runtime(48 * 60_000).as_deref(), Some("48m"));
        assert_eq!(runtime(120 * 60_000).as_deref(), Some("2h"));
        assert_eq!(runtime(102 * 60_000).as_deref(), Some("1h 42m"));
    }

    #[test]
    fn progress_shows_only_for_a_started_unfinished_item() {
        let mut i = item(ItemKind::Movie);
        assert_eq!(progress_percent(&i), None);
        i.runtime_ms = 1000;
        i.position_ms = 1;
        assert_eq!(progress_percent(&i), Some(1));
        i.position_ms = 1000;
        assert_eq!(progress_percent(&i), Some(99));
        i.position_ms = 500;
        assert_eq!(progress_percent(&i), Some(50));
        i.played = true;
        assert_eq!(progress_percent(&i), None);
    }

    #[test]
    fn containers_open_their_listing_and_the_rest_their_title() {
        for kind in [
            ItemKind::Library,
            ItemKind::Series,
            ItemKind::Season,
            ItemKind::Album,
            ItemKind::Artist,
        ] {
            assert!(
                matches!(open_route(&item(kind)), Route::Browse(_)),
                "{kind:?}"
            );
        }
        for kind in [
            ItemKind::Movie,
            ItemKind::Episode,
            ItemKind::Book,
            ItemKind::Other,
        ] {
            assert!(
                matches!(open_route(&item(kind)), Route::Title(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn episodes_carry_their_series_and_numbers() {
        let mut e = item(ItemKind::Episode);
        e.series_name = Some("Show".into());
        e.season = Some(1);
        e.episode = Some(3);
        assert_eq!(sub_lines(&e), ["Show", "S1 · E3"]);
        e.series_name = None;
        assert_eq!(sub_lines(&e), ["S1 · E3"]);
        assert_eq!(sub_lines(&item(ItemKind::Movie)), ["2001"]);
    }

    #[test]
    fn a_tile_puts_series_and_numbers_on_their_own_lines_with_tooltips() {
        let mut e = item(ItemKind::Episode);
        e.name = "Murder House".into();
        e.series_name = Some("American Horror Story".into());
        e.season = Some(1);
        e.episode = Some(1);
        let html = render(move || rsx! { PosterTile { item: e.clone() } });
        for want in [
            r#"class="pharos-poster-name" title="Murder House""#,
            r#"class="pharos-poster-sub" title="American Horror Story""#,
            r#"class="pharos-poster-sub" title="S1 · E1""#,
        ] {
            assert!(html.contains(want), "missing {want} in {html}");
        }
    }

    /// B320: a song with no art of its own still gets a designed tile, and
    /// its page names the artist and links the album.
    #[test]
    fn a_track_without_art_shows_a_tile_its_artist_and_album() {
        let mut song = item(ItemKind::Audio);
        song.name = "Enchanted".into();
        song.image = None;
        song.runtime_ms = 5 * 60_000;
        song.artist = Some("Owl City".into());
        song.album = Some("The Midsummer Station".into());
        song.parent = Some(ItemRef::Synth(9));
        song.season = Some(1);
        song.episode = Some(3);
        let detail = Detail {
            item: song.clone(),
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        for want in [
            "pharos-title-art pharos-title-art-square",
            r#"class="pharos-poster-placeholder pharos-art-placeholder" aria-hidden="true">♪<"#,
            ">Owl City<",
            r#"href="/ui/browse/s9""#,
            ">The Midsummer Station<",
            "2001 · 5m · Track 3",
        ] {
            assert!(html.contains(want), "missing {want} in {html}");
        }

        let html = render(move || rsx! { PosterTile { item: song.clone() } });
        assert!(html.contains(">♪<"), "{html}");
        assert!(
            html.contains(r#"class="pharos-poster-sub" title="Owl City""#),
            "{html}"
        );
    }

    #[test]
    fn a_song_numbers_its_disc_only_past_the_first() {
        let mut song = item(ItemKind::Audio);
        song.episode = Some(4);
        assert_eq!(index_label(&song).as_deref(), Some("Track 4"));
        song.season = Some(1);
        assert_eq!(index_label(&song).as_deref(), Some("Track 4"));
        song.season = Some(2);
        assert_eq!(index_label(&song).as_deref(), Some("Disc 2 · Track 4"));
        assert_eq!(sub_lines(&song), ["2001"], "untagged: the year");
    }
}
