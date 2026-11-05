//! The native UI's screens as pure components: data in through props,
//! navigation out through the [`Nav`] context and marks through
//! [`MarkPlayed`]. No IO here, so every screen
//! renders under `dioxus-ssr` in tests (009 FR-014).

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, HomeRail, Item, ItemKind, ItemRef, LibraryKind, Page};

use super::depth;
use super::platform;
use super::route::Route;
use super::seen::{self, MemberSeen};
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
/// names a whole group it marks, such as "season".
#[component]
pub fn WatchedToggle(id: ItemRef, played: bool, what: Option<String>) -> Element {
    let mark = use_context::<MarkPlayed>();
    let state = if played { "unwatched" } else { "watched" };
    let text = match what {
        Some(what) => format!("Mark {what} {state}"),
        None => format!("Mark {state}"),
    };
    rsx! {
        button {
            r#type: "button",
            class: "pharos-button pharos-button-secondary",
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
    children: Element,
) -> Element {
    let nav = use_context::<Nav>();
    let href = to.path();
    rsx! {
        a {
            href: "{href}",
            class,
            "aria-current": if current { "page" },
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

/// The frame around every signed-in screen: the bar, a button per library
/// and the trail to the page shown (outermost first).
#[component]
pub fn Shell(
    account: Element,
    libraries: Vec<Item>,
    trail: Vec<Item>,
    children: Element,
) -> Element {
    let current = trail
        .first()
        .filter(|item| item.kind == ItemKind::Library)
        .map(|library| library.id);
    rsx! {
        div { class: "pharos-app",
            header { class: "pharos-banner pharos-shell",
                div { class: "pharos-shell-bar",
                    Link { to: Route::Home, class: "pharos-brand", Brand {} }
                    nav { class: "pharos-shell-nav",
                        Link { to: Route::Find, "Find" }
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
                            span { "aria-current": "page", "{item.name}" }
                        } else {
                            Link { to: open_route(&item), "{item.name}" }
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
                    None => rsx! { div { class: "pharos-poster-placeholder", aria_hidden: "true", "{initial(&item.name)}" } },
                }
                if item.played {
                    span { class: "pharos-badge", "Watched" }
                }
                if let Some(p) = progress {
                    div { class: "pharos-progress", style: "width: {p}%" }
                }
                Ratings { kind: item.kind, imdb: item.imdb_rating, critic: item.critic_rating, tile: true }
            }
            span { class: "pharos-poster-name", title: "{item.name}", "{item.name}" }
            for line in sub {
                span { class: "pharos-poster-sub", title: "{line}", "{line}" }
            }
        }
    }
}

#[component]
pub fn TitleView(
    detail: Detail,
    /// The viewer asked to watch this item together.
    #[props(default)]
    on_watch_together: EventHandler<ItemRef>,
    /// The viewer asked to play this part-watched item from the start.
    #[props(default)]
    on_play_from_start: EventHandler<ItemRef>,
    /// What the group's members have seen, when in one.
    #[props(default)]
    seen: Vec<MemberSeen>,
) -> Element {
    let item = &detail.item;
    let in_group = if seen.is_empty() {
        Vec::new()
    } else {
        vec![(String::new(), seen::rows(&seen, item.id))]
    };
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
                Ratings { kind: item.kind, imdb: item.imdb_rating, critic: item.critic_rating, tile: false }
                div { class: "pharos-actions",
                    if item.kind.playable() {
                        Link { to: Route::Watch(item.id), class: "pharos-button",
                            if resume { "Resume" } else { "Play" }
                        }
                    }
                    if item.kind.playable() && resume {
                        {
                            let id = item.id;
                            rsx! {
                                button {
                                    class: "pharos-button pharos-button-secondary",
                                    r#type: "button",
                                    onclick: move |_| on_play_from_start.call(id),
                                    "Play from start"
                                }
                            }
                        }
                    }
                    if can_watch_together(item.kind) {
                        {
                            let id = item.id;
                            rsx! {
                                button {
                                    class: "pharos-button",
                                    r#type: "button",
                                    onclick: move |_| on_watch_together.call(id),
                                    "Watch together"
                                }
                            }
                        }
                    }
                    if matches!(open_route(item), Route::Browse(_)) {
                        Link { to: Route::Browse(item.id), class: "pharos-button", "Open" }
                    }
                    if item.kind.playable() {
                        WatchedToggle { id: item.id, played: item.played }
                    }
                }
                InThisGroup { rows: in_group }
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

/// The lines under a poster's name: an episode's series, then its numbers,
/// each on its own line so a long series name cannot hide the numbers.
fn sub_lines(item: &Item) -> Vec<String> {
    match item.kind {
        ItemKind::Episode => [item.series_name.clone(), episode_label(item)]
            .into_iter()
            .flatten()
            .collect(),
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
        }
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
                    || rsx! { Shell { account: rsx! {}, libraries: vec![], trail: vec![], p { "body" } } },
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
        assert!(html.contains(">Play from start<"), "{html}");

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
        };
        let html = render(move || rsx! { TitleView { detail: detail.clone() } });
        assert!(html.contains(">Mark unwatched<"), "{html}");
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
            };
            let html = render(move || rsx! { TitleView { detail: detail.clone() } });
            assert_eq!(html.contains(">Watch together<"), shown, "{kind:?}: {html}");
        }
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
