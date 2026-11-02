//! The native UI's screens as pure components: data in through props,
//! navigation out through the [`Nav`] context. No IO here, so every screen
//! renders under `dioxus-ssr` in tests (009 FR-014).

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, Item, ItemKind, Page};

use super::playback::MediaEvent;
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
    let empty = home.rails.is_empty() && home.resume.is_empty() && home.next_up.is_empty();
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
            for rail in home.rails.iter() {
                Rail {
                    key: "{rail.library.id}",
                    title: rail.library.name.clone(),
                    link: Route::Browse(rail.library.id),
                    items: rail.latest.clone(),
                }
            }
        }
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

#[component]
pub fn BrowseView(title: String, page: Page) -> Element {
    rsx! {
        section { class: "pharos-browse",
            h2 { "{title}" }
            if page.items.is_empty() {
                p { class: "pharos-empty", "Nothing in here yet." }
            }
            ul { class: "pharos-grid",
                for item in page.items.iter() {
                    li { key: "{item.id}", PosterTile { item: item.clone() } }
                }
            }
            if page.total as usize > page.items.len() {
                p { class: "pharos-muted",
                    "Showing {page.items.len()} of {page.total}."
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
        ItemKind::Library | ItemKind::Series | ItemKind::Season | ItemKind::Album => {
            Route::Browse(item.id)
        }
        _ => Route::Title(item.id),
    }
}

#[component]
pub fn PosterTile(item: Item) -> Element {
    let subtitle = subtitle(&item);
    let progress = progress_percent(&item);
    rsx! {
        Link { to: open_route(&item), class: "pharos-poster",
            div { class: "pharos-poster-art",
                match &item.image {
                    Some(src) => rsx! { img { src: "{src}", alt: "", loading: "lazy", decoding: "async" } },
                    None => rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "{initial(&item.name)}" } },
                }
                if item.played {
                    span { class: "pharos-badge", "Watched" }
                }
                if let Some(p) = progress {
                    div { class: "pharos-progress", style: "width: {p}%" }
                }
            }
            span { class: "pharos-poster-name", "{item.name}" }
            if let Some(s) = subtitle {
                span { class: "pharos-poster-sub", "{s}" }
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
        episode_label(item),
    ]
    .into_iter()
    .flatten()
    .collect();
    let facts = facts.join(" · ");
    let resume = progress_percent(item).is_some();
    rsx! {
        article { class: "pharos-title",
            div { class: "pharos-title-art",
                if let Some(src) = &item.image {
                    img { src: "{src}", alt: "" }
                }
            }
            div { class: "pharos-title-body",
                if let Some(series) = &item.series_name {
                    p { class: "pharos-muted", "{series}" }
                }
                h2 { "{item.name}" }
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

/// The `<video>` the playback engine attaches to.
pub const VIDEO_ID: &str = "pharos-video";

/// The player. The engine gives the `<video>` its stream once mounted;
/// `on_media` hears the events progress reports follow.
#[component]
pub fn WatchView(title: String, on_media: EventHandler<MediaEvent>) -> Element {
    rsx! {
        section { class: "pharos-watch",
            h2 { class: "pharos-watch-title", "{title}" }
            video {
                id: VIDEO_ID,
                class: "pharos-video",
                controls: true,
                autoplay: true,
                playsinline: true,
                onplaying: move |_| on_media.call(MediaEvent::Playing),
                onpause: move |_| on_media.call(MediaEvent::Pause),
                onseeked: move |_| on_media.call(MediaEvent::Seeked),
                ontimeupdate: move |_| on_media.call(MediaEvent::TimeUpdate),
                onended: move |_| on_media.call(MediaEvent::Ended),
            }
        }
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

/// "S1 · E3" for an episode with its numbers.
pub fn episode_label(item: &Item) -> Option<String> {
    match (item.kind, item.season, item.episode) {
        (ItemKind::Episode, Some(s), Some(e)) => Some(format!("S{s} · E{e}")),
        (ItemKind::Episode, None, Some(e)) => Some(format!("E{e}")),
        _ => None,
    }
}

fn subtitle(item: &Item) -> Option<String> {
    match item.kind {
        ItemKind::Episode => match (&item.series_name, episode_label(item)) {
            (Some(series), Some(label)) => Some(format!("{series} · {label}")),
            (Some(series), None) => Some(series.clone()),
            (None, label) => label,
        },
        _ => item.year.map(|y| y.to_string()),
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
            next_up: vec![],
        };
        let html = render(move || rsx! { HomeView { home: home.clone() } });
        assert!(html.contains("Continue watching"), "{html}");
        assert!(!html.contains("Next up"), "{html}");
        assert!(
            html.contains(r#"href="/browse/7""#) && html.contains("Films"),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/title/42""#) && html.contains("Heat"),
            "{html}"
        );
        assert!(html.contains("width: 25%"), "{html}");
    }

    #[test]
    fn an_empty_home_points_at_manage() {
        let home = Home {
            rails: vec![],
            resume: vec![],
            next_up: vec![],
        };
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
        let html =
            render(move || rsx! { BrowseView { title: "Shows".to_string(), page: page.clone() } });
        assert!(html.contains(r#"href="/browse/1""#), "{html}");
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
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(
            html.contains(r#"href="/watch/1""#) && html.contains(">Play<"),
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
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(!html.contains("/watch/"), "{html}");
        assert!(html.contains(r#"href="/browse/1""#), "{html}");
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
        assert_eq!(subtitle(&e).as_deref(), Some("Show · S1 · E3"));
        assert_eq!(subtitle(&item(ItemKind::Movie)).as_deref(), Some("2001"));
    }
}
