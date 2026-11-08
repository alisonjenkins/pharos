//! The native UI's screens as pure components: data in through props,
//! navigation out through the [`Nav`] context and marks through
//! [`MarkPlayed`]. No IO here, so every screen
//! renders under `dioxus-ssr` in tests (009 FR-014).

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind, Page};

use super::art::{Art, POSTER_SIZES};
use super::depth;
use super::find::SUGGEST_LIMIT;
use super::keymap::Mode;
use super::keys::{use_arrival_focus, use_key_mode};
use super::palette::Row;
use super::platform;
use super::playback::{ends_at_label, left_to_watch, start_ms, Left};
use super::route::Route;
use super::seen::{self, MemberSeen};
use super::stage::{fullscreen_for_play, StageCtx};
use super::together::can_watch_together;
use super::together_view::InThisGroup;

/// Navigation, provided by the app root: a click on a [`Link`] calls it
/// instead of reloading the page.
#[derive(Clone, Copy)]
pub struct Nav(pub EventHandler<Route>);

/// Marks an item watched (`true`) or unwatched, provided by the app root:
/// it stores the mark and reloads the screen.
#[derive(Clone, Copy)]
pub struct MarkPlayed(pub EventHandler<(ItemRef, bool)>);

/// Plays an item from 0 s whatever position was saved, provided by the app
/// root.
#[derive(Clone, Copy)]
pub struct PlayFromStart(pub EventHandler<ItemRef>);

/// A button that marks `id` watched, or unwatched when `played`. `what`
/// names a whole group it marks, such as "season". A song is played, not
/// watched (V752).
#[component]
pub fn WatchedToggle(
    id: ItemRef,
    played: bool,
    what: Option<String>,
    #[props(default)] listen: bool,
) -> Element {
    let mark = use_context::<MarkPlayed>();
    let state = match (listen, played) {
        (true, true) => "unplayed",
        (true, false) => "played",
        (false, true) => "unwatched",
        (false, false) => "watched",
    };
    let text = match what {
        Some(what) => format!("Mark {what} {state}"),
        None => format!("Mark {state}"),
    };
    rsx! {
        button {
            r#type: "button",
            class: "pharos-button pharos-button-secondary",
            "data-key-action": "watched",
            onclick: move |_| mark.0.call((id, !played)),
            "{text}"
        }
    }
}

/// An `<a>` to a route. A plain click navigates in place; a modified click
/// (new tab, new window) is left to the browser. `current` marks the link to
/// the page already shown.
#[component]
pub fn Link(
    to: Route,
    class: Option<String>,
    #[props(default)] current: bool,
    /// What the link plays, for a play link: a film or an episode opens in
    /// fullscreen.
    #[props(default)]
    plays: Option<ItemKind>,
    #[props(default)] id: Option<String>,
    /// The key action that presses this link (spec 029), such as `play`.
    #[props(default)]
    key_action: Option<&'static str>,
    children: Element,
) -> Element {
    let nav = use_context::<Nav>();
    let stage = try_use_context::<StageCtx>();
    let href = to.path();
    rsx! {
        a {
            href: "{href}",
            class,
            id,
            "data-key-action": key_action,
            "aria-current": if current { "page" },
            onclick: move |evt: MouseEvent| {
                if evt.modifiers().is_empty() {
                    evt.prevent_default();
                    if let Some(kind) = plays {
                        fullscreen_for_play(kind, stage);
                    }
                    nav.0.call(to.clone());
                }
            },
            {children}
        }
    }
}

/// The Pharos "Beam P" mark (assets/brand/README.md), drawn inline in the
/// theme's own colours: body and beam in the accent, the play button in
/// the accent's foreground, a pair every theme keeps legible (the warm
/// beacon of the standalone files would vanish on High Contrast's yellow).
/// Decorative: the heading beside it carries the name.
#[component]
pub fn BrandMark() -> Element {
    // Each mark owns its gradient: a shared id would resolve every beam to
    // the first mark on the page, and lose it if that one unmounts.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let beam = use_hook(|| {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("pharos-mark-beam-{n}")
    });
    rsx! {
        svg {
            class: "pharos-mark",
            view_box: "0 0 64 64",
            "aria-hidden": "true",
            defs {
                linearGradient {
                    id: "{beam}",
                    x1: "44",
                    x2: "64",
                    y1: "0",
                    y2: "0",
                    "gradientUnits": "userSpaceOnUse",
                    stop { class: "pharos-mark-glow", offset: "0", "stop-opacity": "0.9" }
                    stop { class: "pharos-mark-glow", offset: "1", "stop-opacity": "0" }
                }
            }
            path { d: BRAND_BEAM, fill: "url(#{beam})" }
            path { class: "pharos-mark-body", "fill-rule": "evenodd", d: BRAND_BODY }
            path { class: "pharos-mark-light", d: BRAND_PLAY }
        }
    }
}

const BRAND_BEAM: &str = "M44 22 64 12V36L44 30Z";
const BRAND_BODY: &str = "M12 10a4 4 0 0 1 4-4H32a20 20 0 0 1 0 40H24V56a4 4 0 0 1-4 4H16a4 4 0 0 1-4-4Z M26 16.5V35.5L42 26Z";
const BRAND_PLAY: &str = "M26 16.5V35.5L42 26Z";

/// The mark and the name, as the page heading.
#[component]
fn Brand() -> Element {
    rsx! {
        h1 { class: "pharos-lockup", BrandMark {} span { "pharos" } }
    }
}

/// The frame around every signed-in screen: the bar, its search box, a
/// button per library and the trail to the page shown (outermost first).
#[component]
pub fn Shell(
    account: Element,
    search: Element,
    libraries: Vec<Item>,
    trail: Vec<Item>,
    children: Element,
) -> Element {
    let current = trail
        .first()
        .filter(|item| item.kind == ItemKind::Library)
        .map(|library| library.id);
    rsx! {
        div { class: "pharos-app", "data-keys": "page",
            header { class: "pharos-banner pharos-shell",
                div { class: "pharos-shell-bar",
                    Link { to: Route::Home, class: "pharos-brand", Brand {} }
                    nav { class: "pharos-shell-nav",
                        {search}
                        Link { to: Route::Together, "Together" }
                        {account}
                    }
                }
                LibraryNav { libraries, current }
                Crumbs { trail }
            }
            main { class: "pharos-main", {children} }
        }
    }
}

/// A button per library, in the home page's order.
#[component]
pub fn LibraryNav(libraries: Vec<Item>, current: Option<ItemRef>) -> Element {
    if libraries.is_empty() {
        return rsx! {};
    }
    let mut libraries = libraries;
    libraries.sort_by_key(|library| rail_rank(library.library_kind));
    rsx! {
        nav { class: "pharos-library-nav", aria_label: "Libraries",
            for library in libraries {
                Link {
                    key: "{library.id}",
                    to: Route::Browse(library.id),
                    class: "pharos-library-tab",
                    current: current == Some(library.id),
                    "{library.name}"
                }
            }
        }
    }
}

/// The way down to the page shown, each step a link back up to it. A lone
/// library needs no trail: its button already says where you are.
#[component]
pub fn Crumbs(trail: Vec<Item>) -> Element {
    if trail.len() < 2 {
        return rsx! {};
    }
    let last = trail.len() - 1;
    rsx! {
        nav { class: "pharos-crumbs", aria_label: "Breadcrumb",
            ol {
                for (i, item) in trail.into_iter().enumerate() {
                    li { key: "{item.id}",
                        if i == last {
                            span { "aria-current": "page", {display_name(&item)} }
                        } else {
                            Link { to: open_route(&item), {display_name(&item)} }
                        }
                    }
                }
            }
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
            header { class: "pharos-banner", Brand {} }
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
            header { class: "pharos-banner", Brand {} }
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

/// The group ended: news, not an error, and the film plays on.
#[component]
pub fn GroupEndedNote(message: String) -> Element {
    rsx! {
        p { class: "pharos-status-note", role: "status", "{message}. You can keep watching." }
    }
}

/// Why a video is not playing: the plain-language headline the viewer reads
/// and the raw reason kept for the details line (B806).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayFailure {
    pub headline: &'static str,
    pub reason: String,
}

impl PlayFailure {
    /// The stream broke while it played.
    pub fn stream(reason: impl Into<String>) -> Self {
        Self {
            headline: "This video stopped playing.",
            reason: reason.into(),
        }
    }

    /// The video could not be started: the engine would not attach, or the
    /// server refused the play.
    pub fn start(reason: impl Into<String>) -> Self {
        Self {
            headline: "This video could not be started.",
            reason: reason.into(),
        }
    }

    /// The one line a compact window shows in place of the panel.
    pub fn line(&self) -> String {
        format!("{} {}", self.headline, self.reason)
    }
}

/// A playback failure over the picture: what happened in plain words, a way
/// to try again from the last position, a way out, and the raw reason in a
/// small details line (B806).
#[component]
pub fn PlaybackFailure(
    failure: PlayFailure,
    on_retry: EventHandler<()>,
    on_back: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "pharos-playback-failure", role: "alert",
            p { class: "pharos-playback-failure-title", "{failure.headline}" }
            p { class: "pharos-playback-failure-hint",
                "Check your connection, then try again. Your place is kept."
            }
            div { class: "pharos-playback-failure-actions",
                button {
                    class: "pharos-playback-failure-retry",
                    r#type: "button",
                    // A remote or keyboard lands on the way forward (TV, Quest).
                    autofocus: true,
                    onclick: move |_| on_retry.call(()),
                    "Try again"
                }
                button {
                    class: "pharos-playback-failure-back",
                    r#type: "button",
                    onclick: move |_| on_back.call(()),
                    "Back"
                }
            }
            p { class: "pharos-playback-failure-details", "Details: {failure.reason}" }
        }
    }
}

#[component]
pub fn HomeView(home: Home) -> Element {
    use_hook(|| std::rc::Rc::new(platform::rail_gestures()));
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

/// Where the shared search stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Hits {
    /// Nothing asked yet: the box is blank.
    Idle,
    /// The matches for the newest term; empty when nothing matched.
    Found(Page),
    Failed(String),
}

const SEARCH_INPUT: &str = "search-input";
const SEARCH_LIST: &str = "search-list";

/// The top bar's search box: matches from every library listed under it as
/// you type. `open` hides the list (Escape, a pick, focus gone) without
/// clearing the text; `active` is the match the arrow keys are on, and
/// `on_all` asks for every match, in the palette.
#[component]
pub fn SearchBox(
    text: String,
    hits: Hits,
    open: bool,
    active: Option<usize>,
    /// The placeholder, naming the palette's shortcut.
    hint: String,
    on_input: EventHandler<String>,
    on_key: EventHandler<KeyboardEvent>,
    on_open: EventHandler<()>,
    on_close: EventHandler<()>,
    on_all: EventHandler<()>,
) -> Element {
    let shown = open && !text.trim().is_empty() && hits != Hits::Idle;
    let active_id = active.filter(|_| shown).map(|at| option_id("search", at));
    rsx! {
        div { class: "pharos-search", role: "search",
            input {
                id: SEARCH_INPUT,
                r#type: "search",
                placeholder: "{hint}",
                autocomplete: "off",
                role: "combobox",
                "aria-label": "Search all libraries",
                "aria-autocomplete": "list",
                "aria-controls": SEARCH_LIST,
                "aria-expanded": if shown { "true" } else { "false" },
                "aria-activedescendant": active_id,
                value: "{text}",
                oninput: move |evt: FormEvent| on_input.call(evt.value()),
                onkeydown: move |evt| on_key.call(evt),
                onfocus: move |_| on_open.call(()),
                onblur: move |_| on_close.call(()),
            }
            if shown {
                div {
                    class: "pharos-search-pop",
                    // Keeps focus in the box, so its blur does not close the
                    // list before the click on a match lands.
                    onmousedown: move |evt: MouseEvent| evt.prevent_default(),
                    onclick: move |_| on_close.call(()),
                    match hits {
                        Hits::Idle => rsx! {},
                        Hits::Failed(message) => rsx! { ErrorNote { message } },
                        Hits::Found(page) if page.items.is_empty() => rsx! {
                            p { class: "pharos-empty", role: "status", "No matches" }
                        },
                        Hits::Found(page) => rsx! {
                            ul { id: SEARCH_LIST, role: "listbox", "aria-label": "Matches",
                                for (at, item) in page.items.iter().take(SUGGEST_LIMIT).enumerate() {
                                    li {
                                        key: "{item.id}",
                                        id: option_id("search", at),
                                        role: "option",
                                        "aria-selected": if active == Some(at) { "true" } else { "false" },
                                        SearchHit { item: item.clone() }
                                    }
                                }
                            }
                            if page.total as usize > SUGGEST_LIMIT {
                                button {
                                    class: "pharos-search-all",
                                    onclick: move |_| on_all.call(()),
                                    "See all {page.total} matches"
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

fn option_id(list: &str, at: usize) -> String {
    format!("{list}-option-{at}")
}

pub const PALETTE_INPUT: &str = "palette-input";
const PALETTE_LIST: &str = "palette-list";

/// Scrolls the palette's list so its row `at` is in sight.
pub fn reveal_palette_row(at: usize) {
    platform::reveal(PALETTE_LIST, &option_id("palette", at));
}

/// The Ctrl+K / Cmd+K palette: a box over the page that goes anywhere,
/// listing `rows` under their group headings. `active` is the row Enter
/// opens; `failed` is a search that did not answer.
#[component]
pub fn PaletteView(
    text: String,
    rows: Vec<Row>,
    active: Option<usize>,
    failed: Option<String>,
    on_input: EventHandler<String>,
    on_key: EventHandler<KeyboardEvent>,
    on_pick: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    let active_id = active.map(|at| option_id("palette", at));
    rsx! {
        div { class: "pharos-palette",
            div { class: "pharos-palette-backdrop", onclick: move |_| on_close.call(()) }
            div {
                class: "pharos-palette-box",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Go to or search",
                input {
                    id: PALETTE_INPUT,
                    r#type: "search",
                    placeholder: "Go to a page, or search every library",
                    autocomplete: "off",
                    role: "combobox",
                    "aria-label": "Go to or search",
                    "aria-autocomplete": "list",
                    "aria-controls": PALETTE_LIST,
                    "aria-expanded": "true",
                    "aria-activedescendant": active_id,
                    value: "{text}",
                    oninput: move |evt: FormEvent| on_input.call(evt.value()),
                    onkeydown: move |evt| on_key.call(evt),
                }
                div {
                    id: PALETTE_LIST,
                    class: "pharos-palette-list",
                    role: "listbox",
                    "aria-label": "Places and matches",
                    onclick: move |_| on_pick.call(()),
                    for (group, members) in groups(&rows) {
                        ul { key: "{group}", role: "group", "aria-label": group,
                            li { class: "pharos-palette-group", role: "presentation", "{group}" }
                            for (at, row) in members {
                                li {
                                    key: "{at}",
                                    id: option_id("palette", at),
                                    role: "option",
                                    "aria-selected": if active == Some(at) { "true" } else { "false" },
                                    match row {
                                        Row::Place(place) => rsx! {
                                            Link { to: place.route.clone(), class: "pharos-search-hit",
                                                span { class: "pharos-search-text",
                                                    span { class: "pharos-search-name", "{place.label}" }
                                                    span { class: "pharos-muted", "{place.detail}" }
                                                }
                                            }
                                        },
                                        Row::Search(term) => {
                                            let again = term.clone();
                                            rsx! {
                                                // Searches again in place, so the list's click must not close it.
                                                button {
                                                    class: "pharos-search-hit pharos-palette-again",
                                                    r#type: "button",
                                                    onclick: move |evt: MouseEvent| {
                                                        evt.stop_propagation();
                                                        on_input.call(again.clone());
                                                    },
                                                    span { class: "pharos-search-text",
                                                        span { class: "pharos-search-name", "{term}" }
                                                        span { class: "pharos-muted", "Search" }
                                                    }
                                                }
                                            }
                                        }
                                        Row::Recent(item) | Row::Title(item) => rsx! { SearchHit { item: item.clone() } },
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(message) = failed {
                    ErrorNote { message }
                } else if rows.is_empty() {
                    p { class: "pharos-empty", role: "status", "No matches" }
                }
                p { class: "pharos-palette-keys pharos-muted",
                    "↑↓ to move · Enter to open · Esc to close"
                }
            }
        }
    }
}

/// `rows` cut into runs of one group, each row with its place in `rows`.
fn groups(rows: &[Row]) -> Vec<(&'static str, Vec<(usize, Row)>)> {
    let mut groups: Vec<(&'static str, Vec<(usize, Row)>)> = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        match groups.last_mut() {
            Some((group, members)) if *group == row.group() => members.push((at, row.clone())),
            _ => groups.push((row.group(), vec![(at, row.clone())])),
        }
    }
    groups
}

/// One match in a search list: a thumbnail, the name, and what it is.
#[component]
fn SearchHit(item: Item) -> Element {
    // A season is named only by its number; say whose it is.
    let show = match item.kind {
        ItemKind::Season => item.series_name.clone(),
        _ => None,
    };
    let detail = std::iter::once(kind_name(item.kind).to_owned())
        .chain(show)
        .chain(sub_lines(&item))
        .collect::<Vec<_>>()
        .join(" · ");
    rsx! {
        Link { to: open_route(&item), class: "pharos-search-hit",
            match &item.image {
                Some(src) => rsx! { Art { src: src.clone(), sizes: "2.25rem", class: "pharos-search-thumb" } },
                None => rsx! { span { class: "pharos-search-thumb", "aria-hidden": "true", "{initial(&item.name)}" } },
            }
            span { class: "pharos-search-text",
                span { class: "pharos-search-name", "{item.name}" }
                span { class: "pharos-muted", "{detail}" }
            }
        }
    }
}

/// What an item is, in a word, for a search match.
fn kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Movie => "Film",
        ItemKind::Episode => "Episode",
        ItemKind::Audio => "Song",
        ItemKind::Book => "Book",
        ItemKind::Series => "Series",
        ItemKind::Season => "Season",
        ItemKind::Album => "Album",
        ItemKind::Artist => "Artist",
        ItemKind::Person => "Person",
        ItemKind::Collection => "Collection",
        ItemKind::Playlist => "Playlist",
        ItemKind::Channel => "Channel",
        ItemKind::Library => "Library",
        ItemKind::Other => "Item",
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
        | ItemKind::Artist
        | ItemKind::Collection
        | ItemKind::Playlist => Route::Browse(item.id),
        _ => Route::Title(item.id),
    }
}

#[component]
pub fn PosterTile(item: Item) -> Element {
    let name = display_name(&item);
    let sub = sub_lines(&item);
    let progress = progress_percent(&item);
    rsx! {
        Link { to: open_route(&item), class: "pharos-poster",
            div { class: "pharos-poster-art",
                onpointermove: move |evt| depth::tilt(&evt),
                onpointerleave: move |evt| depth::untilt(&evt),
                match &item.image {
                    Some(src) => rsx! { Art { src: src.clone(), sizes: POSTER_SIZES } },
                    None => rsx! { ArtPlaceholder { kind: item.kind, name: item.name.clone() } },
                }
                if item.played {
                    span { class: "pharos-badge", if item.kind == ItemKind::Audio { "Played" } else { "Watched" } }
                }
                if item.stereo.is_some() {
                    span { class: "pharos-badge pharos-badge-3d", title: "Has a 3D version", "3D" }
                }
                if let Some(p) = progress {
                    div { class: "pharos-progress", style: "width: {p}%" }
                }
                Ratings { kind: item.kind, imdb: item.imdb_rating, critic: item.critic_rating, tile: true }
            }
            span { class: "pharos-poster-name", title: "{name}", "{name}" }
            for line in sub {
                span { class: "pharos-poster-sub", title: "{line}", "{line}" }
            }
        }
    }
}

/// What stands where an item has no art: a song or album gets the music tile
/// (V751), anything else, a music video included, its initial.
#[component]
pub fn ArtPlaceholder(kind: ItemKind, name: String) -> Element {
    if matches!(kind, ItemKind::Audio | ItemKind::Album) {
        rsx! {
            div { class: "pharos-poster-placeholder pharos-art-song", aria_hidden: "true",
                span { class: "pharos-art-note", "♪" }
            }
        }
    } else {
        rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "{initial(&name)}" } }
    }
}

/// The id of a page's main action, focused when the page opens.
pub const MAIN_ACTION: &str = "pharos-main-action";

#[component]
pub fn TitleView(
    detail: Detail,
    /// The viewer asked to watch this item together, from this position.
    #[props(default)]
    on_watch_together: EventHandler<(ItemRef, u64)>,
    /// The viewer asked to play this part-watched item from the start.
    #[props(default)]
    on_play_from_start: EventHandler<ItemRef>,
    /// What the group's members have seen, when in one.
    #[props(default)]
    seen: Vec<MemberSeen>,
    /// The viewer is in a group: the together buttons move it here.
    #[props(default)]
    in_group: bool,
) -> Element {
    use_ends_at_refresh();
    use_key_mode(Mode::Detail);
    use_arrival_focus(detail.item.id, MAIN_ACTION);
    let stage = try_use_context::<StageCtx>();
    let item = &detail.item;
    let group_rows = if seen.is_empty() {
        Vec::new()
    } else {
        vec![(String::new(), seen::rows(&seen, item.id))]
    };
    let facts: Vec<String> = [
        item.year.map(|y| y.to_string()),
        runtime(item.runtime_ms),
        episode_label(item),
        title_ends_at(&detail),
    ]
    .into_iter()
    .flatten()
    .collect();
    let facts = facts.join(" · ");
    let resume = progress_percent(item).is_some();
    rsx! {
        article { class: "pharos-title",
            div { class: "pharos-title-art",
                match &item.image {
                    Some(src) => rsx! { Art { src: src.clone(), sizes: "16rem", lazy: false } },
                    None if item.kind == ItemKind::Audio => rsx! { ArtPlaceholder { kind: item.kind, name: item.name.clone() } },
                    None => rsx! {},
                }
            }
            div { class: "pharos-title-body",
                if let Some(series) = &item.series_name {
                    p { class: "pharos-muted", "{series}" }
                }
                h2 { {display_name(item)} }
                if !facts.is_empty() {
                    p { class: "pharos-muted", "{facts}" }
                }
                Ratings { kind: item.kind, imdb: item.imdb_rating, critic: item.critic_rating, tile: false }
                div { class: "pharos-actions",
                    if item.kind.playable() {
                        Link {
                            to: Route::Watch(item.id),
                            class: "pharos-button",
                            plays: item.kind,
                            id: MAIN_ACTION,
                            key_action: "play",
                            if resume { "Resume" } else { "Play" }
                        }
                    }
                    if item.kind.playable() && resume {
                        {
                            let id = item.id;
                            let kind = item.kind;
                            rsx! {
                                button {
                                    class: "pharos-button pharos-button-secondary",
                                    r#type: "button",
                                    "data-key-action": "restart",
                                    onclick: move |_| {
                                        fullscreen_for_play(kind, stage);
                                        on_play_from_start.call(id);
                                    },
                                    "Play from start"
                                }
                            }
                        }
                    }
                    if can_watch_together(item.kind) {
                        for (label, from, class, key_action) in together_starts(item, in_group) {
                            {
                                let id = item.id;
                                rsx! {
                                    button {
                                        class: "{class}",
                                        r#type: "button",
                                        "data-key-action": key_action,
                                        onclick: move |_| on_watch_together.call((id, from)),
                                        "{label}"
                                    }
                                }
                            }
                        }
                    }
                    if matches!(open_route(item), Route::Browse(_)) {
                        Link { to: Route::Browse(item.id), class: "pharos-button", "Open" }
                    }
                    if item.kind.playable() {
                        WatchedToggle { id: item.id, played: item.played, listen: item.kind == ItemKind::Audio }
                    }
                }
                InThisGroup { rows: group_rows }
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

/// One outside score for a title, as a chip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rating {
    /// `imdb`, `fresh` or `rotten`: picks the chip's colour.
    pub source: &'static str,
    pub label: &'static str,
    pub value: String,
    /// The chip's tooltip, with the source spelled out.
    pub tooltip: String,
}

/// The Tomatometer at which Rotten Tomatoes calls a title fresh.
const FRESH_FROM: u16 = 60;

/// A film's or show's IMDb and Rotten Tomatoes scores, in that order,
/// whichever it has; nothing for any other kind, whose scores (an album's
/// critic score, an episode's show-level one) these chips would misname.
pub fn ratings(kind: ItemKind, imdb: Option<u16>, critic: Option<u16>) -> Vec<Rating> {
    if !matches!(kind, ItemKind::Movie | ItemKind::Series) {
        return Vec::new();
    }
    let imdb = imdb.map(|tenths| {
        let score = format!("{}.{}", tenths / 10, tenths % 10);
        Rating {
            source: "imdb",
            label: "IMDb",
            tooltip: format!("IMDb {score} out of 10"),
            value: score,
        }
    });
    let critics = critic.map(|percent| Rating {
        source: if percent >= FRESH_FROM {
            "fresh"
        } else {
            "rotten"
        },
        label: "RT",
        value: format!("{percent}%"),
        tooltip: format!("Rotten Tomatoes {percent}%"),
    });
    imdb.into_iter().chain(critics).collect()
}

/// A title's outside scores as chips; nothing when it has none. A tile's
/// chips are hidden from assistive tech so they stay out of the link's name.
#[component]
pub fn Ratings(kind: ItemKind, imdb: Option<u16>, critic: Option<u16>, tile: bool) -> Element {
    let ratings = ratings(kind, imdb, critic);
    if ratings.is_empty() {
        return rsx! {};
    }
    let (class, hidden) = match tile {
        true => ("pharos-ratings pharos-ratings-tile", Some("true")),
        false => ("pharos-ratings", None),
    };
    rsx! {
        ul { class, aria_label: "Ratings", "aria-hidden": hidden,
            for r in ratings {
                li { class: "pharos-rating pharos-rating-{r.source}", title: "{r.tooltip}",
                    span { class: "pharos-rating-source", "{r.label}" }
                    " {r.value}"
                }
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

/// The name to show for an item. An episode the library knows no title
/// for is often stored under its show's name or its file name, which the
/// tile already shows beneath; it reads "Episode 7" instead.
pub fn display_name(item: &Item) -> String {
    if item.kind == ItemKind::Episode && is_placeholder_episode_name(item) {
        return match item.episode {
            Some(e) => format!("Episode {e}"),
            None => "Episode".to_owned(),
        };
    }
    item.name.clone()
}

/// An item's name for the tab title: an episode's carries its show's.
pub fn page_name(item: &Item) -> String {
    match (&item.kind, &item.series_name) {
        (ItemKind::Episode, Some(series)) => format!("{} · {series}", display_name(item)),
        _ => display_name(item),
    }
}

/// Empty, the show's own name, or a file name with an `S01E02` in it; the
/// server's `apply_enrichment` replaces the same stand-ins.
pub fn is_placeholder_episode_name(item: &Item) -> bool {
    let fold = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let name = fold(&item.name);
    name.is_empty()
        || item.series_name.as_deref().is_some_and(|s| fold(s) == name)
        || has_season_episode_token(&item.name)
}

/// `S01E02` / `s1e2` at a word start, as a release or Sonarr file name has.
fn has_season_episode_token(name: &str) -> bool {
    let b = name.as_bytes();
    (0..b.len()).any(|i| {
        let starts_word = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
        if !starts_word || !b[i].eq_ignore_ascii_case(&b's') {
            return false;
        }
        let s = digits(i + 1);
        let e_at = i + 1 + s;
        s > 0 && b.get(e_at).is_some_and(|c| c.eq_ignore_ascii_case(&b'e')) && digits(e_at + 1) > 0
    })
}

/// The lines under a poster's name: an episode's series, then its numbers,
/// each on its own line so a long series name cannot hide the numbers. A
/// song names its artist, or its year when the file has no artist tag (V750).
fn sub_lines(item: &Item) -> Vec<String> {
    match item.kind {
        ItemKind::Episode => [item.series_name.clone(), episode_label(item)]
            .into_iter()
            .flatten()
            .collect(),
        ItemKind::Audio if item.artist.is_some() => item.artist.clone().into_iter().collect(),
        _ => item.year.map(|y| y.to_string()).into_iter().collect(),
    }
}

/// The player's heading: the show's name over "S1 · E22 · Title" for an
/// episode, a film's name over its year. An episode with only a stand-in
/// name drops it, since the numbers already say "E22".
pub fn watch_heading(item: &Item) -> (String, Option<String>) {
    match (item.kind, &item.series_name) {
        (ItemKind::Episode, Some(series)) => {
            let name = (!is_placeholder_episode_name(item)).then(|| item.name.clone());
            let line: Vec<String> = [episode_label(item), name].into_iter().flatten().collect();
            (series.clone(), (!line.is_empty()).then(|| line.join(" · ")))
        }
        (ItemKind::Episode, None) => (display_name(item), episode_label(item)),
        _ => (display_name(item), item.year.map(|y| y.to_string())),
    }
}

/// When a film or episode would end if played now, from where Play starts.
fn title_ends_at(detail: &Detail) -> Option<String> {
    let item = &detail.item;
    if !matches!(item.kind, ItemKind::Movie | ItemKind::Episode) {
        return None;
    }
    let from = start_ms(item.position_ms, item.played);
    ends_at(left_to_watch(&detail.segments, from, item.runtime_ms))
}

/// How often an "Ends at" estimate is worked out again when nothing else
/// redraws it, as on a title page or in a paused player.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
const ENDS_AT_REFRESH_MS: u32 = 30_000;

/// Redraws the calling component every [`ENDS_AT_REFRESH_MS`], so an
/// "Ends at" it shows keeps up with the clock.
pub fn use_ends_at_refresh() {
    let tick = use_signal(|| 0u32);
    // The host's sleep returns at once, so this would spin there.
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    use_future(move || async move {
        let mut tick = tick;
        loop {
            platform::sleep_ms(ENDS_AT_REFRESH_MS).await;
            let next = tick.peek().wrapping_add(1);
            tick.set(next);
        }
    });
    tick();
}

/// "Ends at 10:42 PM": the local time `left` runs out if watched from now.
pub fn ends_at(left: Option<Left>) -> Option<String> {
    let left = left?;
    let time = platform::local_time(platform::wall_clock_ms() + left.ms as f64)?;
    Some(ends_at_label(&time, left.before_credits))
}

/// The title page's ways to start a group on `item`, or, `in_group`, to
/// move the viewer's group to it (014 US10): label, start position and
/// button class. A part-watched title starts where the viewer left off, or
/// over (B556/V466).
fn together_starts(
    item: &Item,
    in_group: bool,
) -> Vec<(&'static str, u64, &'static str, &'static str)> {
    let (resume, from_start, fresh) = if in_group {
        (
            "Resume for the group",
            "Play for the group from start",
            "Play for the group",
        )
    } else {
        (
            "Resume together",
            "Watch together from start",
            "Watch together",
        )
    };
    if progress_percent(item).is_some() {
        vec![
            (
                resume,
                start_ms(item.position_ms, item.played),
                "pharos-button",
                "together",
            ),
            (
                from_start,
                0,
                "pharos-button pharos-button-secondary",
                "together-start",
            ),
        ]
    } else {
        vec![(fresh, 0, "pharos-button", "together")]
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

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
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
        }
    }

    #[test]
    fn the_player_heading_names_the_show_season_and_episode() {
        let episode = Item {
            name: "Future Enemies".into(),
            series_name: Some("Frieren".into()),
            season: Some(1),
            episode: Some(22),
            ..item(ItemKind::Episode)
        };
        assert_eq!(
            watch_heading(&episode),
            (
                "Frieren".to_owned(),
                Some("S1 · E22 · Future Enemies".to_owned())
            )
        );
        let stand_in = Item {
            name: "Frieren S01E22".into(),
            ..episode.clone()
        };
        assert_eq!(
            watch_heading(&stand_in),
            ("Frieren".to_owned(), Some("S1 · E22".to_owned()))
        );
        let no_show = Item {
            series_name: None,
            ..episode
        };
        assert_eq!(
            watch_heading(&no_show),
            ("Future Enemies".to_owned(), Some("S1 · E22".to_owned()))
        );
        let film = Item {
            name: "Dune".into(),
            ..item(ItemKind::Movie)
        };
        assert_eq!(
            watch_heading(&film),
            ("Dune".to_owned(), Some("2001".to_owned()))
        );
        let undated = Item { year: None, ..film };
        assert_eq!(watch_heading(&undated), ("Dune".to_owned(), None));
    }

    #[test]
    fn a_title_in_a_group_says_who_has_seen_it() {
        let detail = Detail {
            item: item(ItemKind::Movie),
            overview: String::new(),
            genres: Vec::new(),
            chapters: Vec::new(),
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            trickplay: Vec::new(),
            segments: Vec::new(),
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let answer = vec![
            MemberSeen {
                member: pharos_client::group::GroupMember {
                    id: "a".into(),
                    name: "Ada".into(),
                    buffering: false,
                    own_tracks: false,
                },
                items: vec![seen::ItemSeen {
                    id: ItemRef::Real(1),
                    played: false,
                    position_pct: 40,
                    played_count: 0,
                    total_count: 0,
                }],
            },
            MemberSeen {
                member: pharos_client::group::GroupMember {
                    id: "b".into(),
                    name: "Bo".into(),
                    buffering: false,
                    own_tracks: false,
                },
                items: vec![],
            },
        ];
        let html = render({
            let detail = detail.clone();
            move || rsx! { TitleView { detail: detail.clone(), seen: answer.clone() } }
        });
        assert!(html.contains("In this group"), "{html}");
        assert!(html.contains("Ada") && html.contains("40%"), "{html}");
        assert!(html.contains("Bo") && html.contains("not seen"), "{html}");
        let alone = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(!alone.contains("In this group"), "{alone}");
    }

    #[test]
    fn ratings_show_imdb_then_rotten_tomatoes() {
        assert!(ratings(ItemKind::Movie, None, None).is_empty());
        let r = ratings(ItemKind::Movie, Some(78), Some(92));
        assert_eq!(
            r.iter()
                .map(|r| (r.source, r.value.as_str()))
                .collect::<Vec<_>>(),
            [("imdb", "7.8"), ("fresh", "92%")]
        );
        assert_eq!(r[0].tooltip, "IMDb 7.8 out of 10");
        assert_eq!(r[1].tooltip, "Rotten Tomatoes 92%");
    }

    #[test]
    fn a_whole_imdb_score_keeps_its_tenth_and_59_percent_is_rotten() {
        let r = ratings(ItemKind::Series, Some(80), Some(59));
        assert_eq!(r[0].value, "8.0");
        assert_eq!(r[1].source, "rotten");
        assert_eq!(ratings(ItemKind::Series, None, Some(60))[0].source, "fresh");
    }

    #[test]
    fn only_films_and_shows_get_rating_chips() {
        for kind in [
            ItemKind::Episode,
            ItemKind::Album,
            ItemKind::Artist,
            ItemKind::Audio,
        ] {
            assert!(ratings(kind, Some(70), Some(70)).is_empty(), "{kind:?}");
        }
    }

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom = VirtualDom::new_with_props(
            |body: std::rc::Rc<dyn Fn() -> Element>| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                use_context_provider(|| MarkPlayed(EventHandler::new(|_: (ItemRef, bool)| {})));
                body()
            },
            body,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// B806: a failure over the picture names what happened, offers Try
    /// again and Back, and keeps the raw reason as small print.
    #[test]
    fn a_playback_failure_offers_retry_back_and_the_raw_reason() {
        let html = render(|| {
            rsx! {
                PlaybackFailure {
                    failure: PlayFailure::stream("the stream failed (video segment 14: timeout)"),
                    on_retry: |_| {},
                    on_back: |_| {},
                }
            }
        });
        assert!(html.contains("This video stopped playing."), "{html}");
        assert!(
            html.contains("Try again") && html.contains(">Back<"),
            "{html}"
        );
        assert!(
            html.contains("Details: the stream failed (video segment 14: timeout)"),
            "{html}"
        );
        assert!(html.contains(r#"role="alert""#), "{html}");
    }

    #[test]
    fn a_group_that_ended_is_a_status_not_an_alert() {
        let html = render(|| rsx! { GroupEndedNote { message: "This group has ended" } });
        assert!(html.contains(r#"role="status""#), "{html}");
        assert!(
            !html.contains("pharos-error") && !html.contains("alert"),
            "{html}"
        );
        assert!(
            html.contains("This group has ended. You can keep watching."),
            "{html}"
        );
    }

    #[test]
    fn a_start_failure_reads_differently_from_a_broken_stream() {
        let start = PlayFailure::start("no MSE");
        assert_ne!(start.headline, PlayFailure::stream("x").headline);
        assert_eq!(start.line(), "This video could not be started. no MSE");
    }

    /// The heading on `html`, open tag to close tag.
    fn heading(html: &str) -> &str {
        let start = html
            .find("<h1")
            .unwrap_or_else(|| panic!("no heading in {html}"));
        let end = html
            .find("</h1>")
            .unwrap_or_else(|| panic!("unclosed heading in {html}"));
        html.get(start..end).unwrap_or_default()
    }

    #[test]
    fn every_header_carries_the_brand_mark_and_name() {
        let pages = [
            (
                "sign-in",
                render(|| rsx! { SignInView { error: None, busy: false, on_submit: |_| {} } }),
            ),
            (
                "status",
                render(|| rsx! { StatusView { title: "Connecting", detail: "…", action: None } }),
            ),
            (
                "shell",
                render(
                    || rsx! { Shell { account: rsx! {}, search: rsx! {}, libraries: vec![], trail: vec![], p { "body" } } },
                ),
            ),
        ];
        for (page, html) in &pages {
            let h1 = heading(html);
            assert!(h1.contains(r#"class="pharos-mark""#), "{page}: {h1}");
            assert!(h1.contains(r#"aria-hidden="true""#), "{page}: {h1}");
            assert!(h1.contains("<span>pharos</span>"), "{page}: {h1}");
            // Themed by class, so no colour is baked in.
            assert!(h1.contains(r#"class="pharos-mark-body""#), "{page}: {h1}");
            assert!(!h1.contains("fill=\"#"), "{page}: {h1}");
        }
    }

    #[test]
    fn two_marks_on_one_page_keep_their_own_beam() {
        let html = render(|| rsx! { BrandMark {} BrandMark {} });
        let ids: Vec<&str> = html
            .match_indices(r#"id="pharos-mark-beam-"#)
            .filter_map(|(at, _)| html.get(at + 4..).and_then(|r| r.split('"').next()))
            .collect();
        assert_eq!(ids.len(), 2, "{html}");
        assert_ne!(ids.first(), ids.last(), "{html}");
        for id in &ids {
            assert!(
                html.contains(&format!("url(#{id})")),
                "{id} unused in {html}"
            );
        }
    }

    /// The inline mark and the shipped files are one drawing.
    #[test]
    fn the_inline_mark_matches_the_shipped_svgs() {
        for (name, svg) in [
            ("mark.svg", include_str!("../../assets/brand/mark.svg")),
            (
                "favicon.svg",
                include_str!("../../assets/brand/favicon.svg"),
            ),
            ("logo.svg", include_str!("../../assets/brand/logo.svg")),
            (
                "logo-light.svg",
                include_str!("../../assets/brand/logo-light.svg"),
            ),
        ] {
            for d in [BRAND_BEAM, BRAND_BODY, BRAND_PLAY] {
                assert!(svg.contains(d), "{name} lacks {d}");
            }
        }
    }

    /// Every icon index.html and the manifest name ships in assets/brand,
    /// so the tab and home-screen icons never 404.
    #[test]
    fn every_linked_brand_file_exists() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/brand");
        let index = include_str!("../../index.html");
        let manifest = include_str!("../../assets/brand/manifest.webmanifest");
        let mut seen = 0;
        for text in [index, manifest] {
            for (at, _) in text.match_indices("/ui/assets/brand/") {
                let rest = text.get(at + "/ui/assets/brand/".len()..).unwrap_or("");
                let file: String = rest.chars().take_while(|c| *c != '"').collect();
                assert!(dir.join(&file).is_file(), "missing assets/brand/{file}");
                // The server indexes the bundle by lowercase key.
                assert_eq!(file, file.to_lowercase(), "{file}");
                seen += 1;
            }
        }
        assert!(seen >= 8, "only {seen} brand links found");
    }

    fn search_box(text: &str, hits: Hits, open: bool, active: Option<usize>) -> String {
        let text = text.to_owned();
        render(move || {
            rsx! {
                SearchBox {
                    text: text.clone(),
                    hits: hits.clone(),
                    open,
                    active,
                    hint: "Search (Ctrl+K)".to_owned(),
                    on_input: |_| {},
                    on_key: |_| {},
                    on_open: |_| {},
                    on_close: |_| {},
                    on_all: |_| {},
                }
            }
        })
    }

    fn found(count: u32) -> Hits {
        let items = (0..count)
            .map(|n| Item {
                id: ItemRef::Real(u64::from(n) + 1),
                name: format!("Match {n}"),
                ..item(ItemKind::Movie)
            })
            .collect();
        Hits::Found(Page {
            items,
            total: count,
            start: 0,
        })
    }

    #[test]
    fn the_shell_bar_holds_the_search_box() {
        let html = render(|| {
            rsx! {
                Shell {
                    account: rsx! {},
                    libraries: vec![],
                    trail: vec![],
                    search: rsx! { SearchBox {
                        text: String::new(),
                        hits: Hits::Idle,
                        open: false,
                        active: None,
                        hint: String::new(),
                        on_input: |_| {},
                        on_key: |_| {},
                        on_open: |_| {},
                        on_close: |_| {},
                        on_all: |_| {},
                    } },
                    p { "screen" }
                }
            }
        });
        let bar = html.split("</header>").next().unwrap_or_default();
        assert!(bar.contains(r#"role="search""#), "{html}");
        assert!(
            bar.contains(r#"aria-label="Search all libraries""#),
            "{html}"
        );
    }

    #[test]
    fn the_search_list_shows_the_first_matches_and_a_way_to_the_rest() {
        let html = search_box("match", found(9), true, Some(1));
        assert_eq!(
            html.matches(r#"role="option""#).count(),
            SUGGEST_LIMIT,
            "{html}"
        );
        assert!(html.contains("See all 9 matches"), "{html}");
        assert!(html.contains(r#"href="/ui/title/1""#), "{html}");
        assert!(html.contains("Film · 2001"), "{html}");
        assert!(
            html.contains(r#"aria-activedescendant="search-option-1""#),
            "{html}"
        );
        assert!(html.contains(r#"aria-expanded="true""#), "{html}");
    }

    #[test]
    fn the_search_list_stays_shut_until_there_is_something_to_show() {
        for (text, hits, open) in [
            ("match", found(3), false),
            ("", found(3), true),
            ("  ", found(3), true),
            ("match", Hits::Idle, true),
        ] {
            let html = search_box(text, hits, open, Some(0));
            assert!(
                !html.contains("pharos-search-pop"),
                "{text:?} {open}: {html}"
            );
            assert!(html.contains(r#"aria-expanded="false""#), "{html}");
            assert!(!html.contains("aria-activedescendant"), "{html}");
        }
    }

    #[test]
    fn the_search_list_offers_the_rest_only_when_there_are_more() {
        let html = search_box("match", found(3), true, None);
        assert!(!html.contains("See all"), "{html}");
        assert!(html.contains(r#"placeholder="Search (Ctrl+K)""#), "{html}");
    }

    #[test]
    fn the_palette_heads_each_group_and_marks_the_active_row() {
        use super::super::palette::{places, Row};
        let film = Item {
            name: "Alpha".into(),
            ..item(ItemKind::Movie)
        };
        let mut rows: Vec<Row> = places(&[]).into_iter().map(Row::Place).collect();
        rows.push(Row::Title(film));
        let html = render(move || {
            rsx! {
                PaletteView {
                    text: "a".to_owned(),
                    rows: rows.clone(),
                    active: Some(1),
                    failed: None,
                    on_input: |_| {},
                    on_key: |_| {},
                    on_pick: |_| {},
                    on_close: |_| {},
                }
            }
        });
        assert!(html.contains(r#"role="dialog""#), "{html}");
        assert_eq!(html.matches("pharos-palette-group").count(), 2, "{html}");
        assert!(
            html.contains(">Go to<") && html.contains(">Titles<"),
            "{html}"
        );
        assert!(html.contains(r#"href="/ui/settings/appearance""#), "{html}");
        assert!(html.contains(r#"href="/ui/title/1""#), "{html}");
        assert!(
            html.contains(r#"aria-activedescendant="palette-option-1""#),
            "{html}"
        );
    }

    #[test]
    fn the_palette_shows_a_failed_search_as_an_alert_and_says_when_nothing_matched() {
        let palette = |failed: Option<String>| {
            render(move || {
                rsx! {
                    PaletteView {
                        text: "zzz".to_owned(),
                        rows: Vec::new(),
                        active: None,
                        failed: failed.clone(),
                        on_input: |_| {},
                        on_key: |_| {},
                        on_pick: |_| {},
                        on_close: |_| {},
                    }
                }
            })
        };
        let html = palette(Some("the server is busy".into()));
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("the server is busy"), "{html}");
        assert!(palette(None).contains("No matches"));
    }

    #[test]
    fn the_search_list_says_when_nothing_matched() {
        let html = search_box("zzz", found(0), true, None);
        assert!(html.contains("No matches"), "{html}");
        assert!(!html.contains(r#"role="option""#), "{html}");
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

    /// Spec 022 US1 (V722): a 3D title is badged on its tile, a 2D one is not.
    #[test]
    fn a_3d_title_is_badged() {
        use pharos_client::api::Stereo;
        let mut movie = item(ItemKind::Movie);
        let flat = render({
            let movie = movie.clone();
            move || rsx! { PosterTile { item: movie.clone() } }
        });
        assert!(!flat.contains("pharos-badge-3d"), "{flat}");
        movie.stereo = Some(Stereo::HalfSideBySide);
        let tile = render(move || rsx! { PosterTile { item: movie.clone() } });
        assert!(
            tile.contains("pharos-badge-3d") && tile.contains(">3D<"),
            "{tile}"
        );
    }

    #[test]
    fn a_title_offers_play_or_resume_play_from_start_and_a_watched_mark() {
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
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(
            html.contains(r#"href="/ui/watch/1""#) && html.contains(">Play<"),
            "{html}"
        );
        assert!(html.contains(">Mark watched<"), "{html}");
        assert!(
            !html.contains("Play from start"),
            "nothing to resume: {html}"
        );
        assert!(
            html.contains(">Watch together<") && !html.contains("Resume together"),
            "nothing to resume together: {html}"
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
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(html.contains(">Resume<"), "{html}");
        assert!(html.contains(">Play from start<"), "{html}");
        assert!(html.contains(">Resume together<"), "{html}");
        assert!(html.contains(">Watch together from start<"), "{html}");

        let mut watched = item(ItemKind::Episode);
        watched.played = true;
        let detail = Detail {
            item: watched,
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(html.contains(">Mark unwatched<"), "{html}");
    }

    /// V466: Resume together starts the group at the saved position, the
    /// other choices at 0. Disarm check: with Resume together starting at 0,
    /// this fails.
    #[test]
    fn a_group_starts_where_the_viewer_chose() {
        let mut film = item(ItemKind::Movie);
        film.runtime_ms = 100 * 60_000;
        let starts = |i: &Item| {
            together_starts(i, false)
                .into_iter()
                .map(|(label, from, _, _)| (label, from))
                .collect::<Vec<_>>()
        };
        assert_eq!(starts(&film), vec![("Watch together", 0)]);
        film.position_ms = 60_000;
        assert_eq!(
            starts(&film),
            vec![
                ("Resume together", 60_000),
                ("Watch together from start", 0)
            ]
        );
    }

    /// 014 US10: in a group, the together buttons move the group to this
    /// title instead of starting another group, from the same positions.
    #[test]
    fn in_a_group_the_together_buttons_play_for_the_group() {
        let mut film = item(ItemKind::Movie);
        film.runtime_ms = 100 * 60_000;
        let starts = |i: &Item| {
            together_starts(i, true)
                .into_iter()
                .map(|(label, from, _, _)| (label, from))
                .collect::<Vec<_>>()
        };
        assert_eq!(starts(&film), vec![("Play for the group", 0)]);
        film.position_ms = 60_000;
        assert_eq!(
            starts(&film),
            vec![
                ("Resume for the group", 60_000),
                ("Play for the group from start", 0)
            ]
        );
        let detail = Detail {
            item: film,
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone(), in_group: true } });
        assert!(html.contains(">Resume for the group<"), "{html}");
        assert!(!html.contains("together<"), "{html}");
    }

    #[test]
    fn watch_together_shows_only_for_films_and_episodes() {
        for (kind, shown) in [
            (ItemKind::Movie, true),
            (ItemKind::Episode, true),
            (ItemKind::Audio, false),
            (ItemKind::Series, false),
            (ItemKind::Album, false),
        ] {
            let detail = Detail {
                item: item(kind),
                overview: String::new(),
                genres: vec![],
                chapters: vec![],
                audio_tracks: vec![],
                subtitle_tracks: vec![],
                trickplay: vec![],
                segments: vec![],
                fonts: Vec::new(),
                artists: Vec::new(),
                album: None,
                versions: Vec::new(),
            };
            let html = render(move || rsx! { TitleView { detail: detail.clone() } });
            assert_eq!(html.contains(">Watch together<"), shown, "{kind:?}: {html}");
        }
    }

    #[test]
    fn a_title_says_when_it_ends_and_whether_that_is_before_the_credits() {
        use pharos_client::api::{Segment, SegmentKind};
        let detail = |kind, segments| {
            let mut i = item(kind);
            i.runtime_ms = 42 * 60_000;
            Detail {
                item: i,
                overview: String::new(),
                genres: vec![],
                chapters: vec![],
                audio_tracks: vec![],
                subtitle_tracks: vec![],
                trickplay: vec![],
                segments,
                fonts: vec![],
                artists: Vec::new(),
                album: None,
                versions: Vec::new(),
            }
        };
        let outro = Segment {
            kind: SegmentKind::Outro,
            start_ms: 40 * 60_000,
            end_ms: 42 * 60_000,
        };
        let shown = |d: Detail| render(move || rsx! { TitleView { detail: d.clone() } });

        let html = shown(detail(ItemKind::Episode, vec![outro]));
        assert!(
            html.contains("· Ends at ") && html.contains(" (before credits)"),
            "{html}"
        );
        let html = shown(detail(ItemKind::Movie, vec![]));
        assert!(
            html.contains("· Ends at ") && !html.contains("credits"),
            "{html}"
        );
        let html = shown(detail(ItemKind::Audio, vec![]));
        assert!(!html.contains("Ends at"), "{html}");
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
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(
            !html.contains("/watch/") && !html.contains("Mark "),
            "{html}"
        );
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
            // B810: a boxset or playlist has no title page, only members.
            ItemKind::Collection,
            ItemKind::Playlist,
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
    fn an_episode_without_a_title_reads_as_its_number() {
        let mut e = item(ItemKind::Episode);
        e.series_name = Some("Star Trek The Next Generation".into());
        e.season = Some(3);
        e.episode = Some(7);
        for stand_in in [
            "Star Trek: The Next Generation",
            "Star Trek - The Next Generation - S03E07 - The Enemy WEBRip-1080p",
            "Person.of.Interest.S01E03.720p.HDTV.x264-IMMERSE",
            "",
        ] {
            e.name = stand_in.into();
            assert_eq!(display_name(&e), "Episode 7", "{stand_in}");
        }
        e.name = "The Enemy".into();
        assert_eq!(display_name(&e), "The Enemy");
        e.name = "Mass Effect".into();
        assert_eq!(display_name(&e), "Mass Effect");

        let mut film = item(ItemKind::Movie);
        film.name = "Show.S01E01".into();
        assert_eq!(display_name(&film), "Show.S01E01");
    }

    #[test]
    fn an_episode_page_is_named_with_its_show() {
        let mut e = item(ItemKind::Episode);
        e.name = "Pilot".into();
        e.series_name = Some("Breaking Bad".into());
        assert_eq!(page_name(&e), "Pilot · Breaking Bad");
        e.series_name = None;
        assert_eq!(page_name(&e), "Pilot");
        let mut film = item(ItemKind::Movie);
        film.name = "Alien".into();
        assert_eq!(page_name(&film), "Alien");
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

    /// A tile shows its IMDb and Rotten Tomatoes scores, decorative beside
    /// the title; a tile with neither shows no ratings row.
    #[test]
    fn a_poster_tile_shows_its_imdb_and_rotten_tomatoes_scores() {
        let mut rated = item(ItemKind::Movie);
        rated.name = "Rated".into();
        rated.imdb_rating = Some(81);
        rated.critic_rating = Some(45);
        let html = render(move || rsx! { PosterTile { item: rated.clone() } });
        assert_eq!(html.matches("pharos-ratings-tile").count(), 1, "{html}");
        assert!(html.contains(r#"aria-hidden="true""#), "{html}");
        assert!(html.contains("pharos-rating pharos-rating-imdb"), "{html}");
        assert!(html.contains(r#"title="IMDb 8.1 out of 10""#), "{html}");
        assert!(
            html.contains("pharos-rating pharos-rating-rotten"),
            "{html}"
        );
        assert!(html.contains(r#"title="Rotten Tomatoes 45%""#), "{html}");

        let mut unrated = item(ItemKind::Movie);
        unrated.name = "Unrated".into();
        let html = render(move || rsx! { PosterTile { item: unrated.clone() } });
        assert!(!html.contains("pharos-ratings-tile"), "{html}");
    }

    /// B800 (V750): a song's card names its artist, falling back to the
    /// year when the file has no artist tag; a film keeps its year.
    #[test]
    fn a_song_card_names_its_artist_or_else_its_year() {
        let mut song = item(ItemKind::Audio);
        song.name = "Teardrop".into();
        song.artist = Some("Massive Attack".into());
        let html = render(move || rsx! { PosterTile { item: song.clone() } });
        assert!(
            html.contains(r#"class="pharos-poster-sub" title="Massive Attack""#),
            "{html}"
        );
        assert!(!html.contains(">2001<"), "{html}");

        let untagged = item(ItemKind::Audio);
        let html = render(move || rsx! { PosterTile { item: untagged.clone() } });
        assert!(html.contains(r#"title="2001""#), "{html}");

        let mut film = item(ItemKind::Movie);
        film.artist = Some("Ignored".into());
        let html = render(move || rsx! { PosterTile { item: film.clone() } });
        assert!(
            html.contains(r#"title="2001""#) && !html.contains("Ignored"),
            "{html}"
        );
    }

    /// B802 (V752): a song is marked played, not watched, on its title page
    /// and its card; a film keeps "watched".
    #[test]
    fn a_song_is_played_not_watched() {
        let mut song = item(ItemKind::Audio);
        song.played = true;
        let card = render({
            let song = song.clone();
            move || rsx! { PosterTile { item: song.clone() } }
        });
        assert!(
            card.contains(">Played<") && !card.contains("Watched"),
            "{card}"
        );
        song.played = false;
        let detail = Detail {
            item: song,
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let page = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(
            page.contains(">Mark played<") && !page.contains(">Mark watched<"),
            "{page}"
        );

        let mut film = item(ItemKind::Movie);
        film.played = true;
        let card = render(move || rsx! { PosterTile { item: film.clone() } });
        assert!(card.contains(">Watched<"), "{card}");
    }

    /// B801 (V751): a song with no cover gets the music tile, on its card and
    /// its title page, as does an album's card; a film with no poster, a
    /// music video included, keeps its initial.
    #[test]
    fn a_song_without_a_cover_shows_the_music_tile() {
        let song = item(ItemKind::Audio);
        let card = render({
            let song = song.clone();
            move || rsx! { PosterTile { item: song.clone() } }
        });
        assert!(
            card.contains("pharos-art-song") && card.contains("♪"),
            "{card}"
        );
        let detail = Detail {
            item: song,
            overview: String::new(),
            genres: vec![],
            chapters: vec![],
            audio_tracks: vec![],
            subtitle_tracks: vec![],
            trickplay: vec![],
            segments: vec![],
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            versions: Vec::new(),
        };
        let page = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(page.contains("pharos-art-song"), "{page}");

        let album = item(ItemKind::Album);
        let card = render(move || rsx! { PosterTile { item: album.clone() } });
        assert!(card.contains("pharos-art-song"), "{card}");

        let film = item(ItemKind::Movie);
        let card = render(move || rsx! { PosterTile { item: film.clone() } });
        assert!(
            !card.contains("pharos-art-song") && card.contains(">N<"),
            "{card}"
        );
    }

    fn library(id: u64, kind: LibraryKind, name: &str) -> Item {
        let mut l = item(ItemKind::Library);
        l.id = ItemRef::Synth(id);
        l.library_kind = Some(kind);
        l.name = name.into();
        l
    }

    #[test]
    fn library_buttons_follow_the_home_order_and_mark_the_one_shown() {
        let libraries = vec![
            library(1, LibraryKind::Music, "Music"),
            library(2, LibraryKind::Movies, "Movies"),
            library(3, LibraryKind::TvShows, "TV Shows"),
        ];
        let html = render(move || {
            rsx! { LibraryNav { libraries: libraries.clone(), current: Some(ItemRef::Synth(2)) } }
        });
        let at = |name: &str| html.find(&format!(">{name}<")).expect(name);
        assert!(
            at("TV Shows") < at("Movies") && at("Movies") < at("Music"),
            "{html}"
        );
        assert!(html.contains(r#"href="/ui/browse/s2""#), "{html}");
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        let current = html.find(r#"aria-current="page""#).unwrap();
        assert!(current < at("Movies") && current > at("TV Shows"), "{html}");
    }

    #[test]
    fn crumbs_link_every_step_but_the_page_shown() {
        let mut series = item(ItemKind::Series);
        series.id = ItemRef::Synth(5);
        series.name = "Breaking Bad".into();
        let mut season = item(ItemKind::Season);
        season.id = ItemRef::Synth(6);
        season.name = "Season 1".into();
        let mut episode = item(ItemKind::Episode);
        episode.id = ItemRef::Real(7);
        episode.name = "Pilot".into();
        let trail = vec![
            library(1, LibraryKind::TvShows, "TV Shows"),
            series,
            season,
            episode,
        ];
        let html = render(move || rsx! { Crumbs { trail: trail.clone() } });
        for want in [
            r#"href="/ui/browse/s1""#,
            r#"href="/ui/browse/s5""#,
            r#"href="/ui/browse/s6""#,
            r#"<span aria-current="page">Pilot</span>"#,
        ] {
            assert!(html.contains(want), "missing {want} in {html}");
        }
        assert!(!html.contains("/ui/title/7"), "{html}");
    }

    #[test]
    fn a_library_alone_needs_no_crumbs() {
        let trail = vec![library(1, LibraryKind::Movies, "Movies")];
        let html = render(move || rsx! { Crumbs { trail: trail.clone() } });
        assert!(!html.contains("pharos-crumbs"), "{html}");
    }
}
