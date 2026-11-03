//! The browse screen's layouts (012 US2, US3, US5): what a library, series,
//! season, artist or album shows, chosen from its detail. A TV library is a
//! grid of series; a series or season is the show page, seasons as tabs over
//! an episode list; a music library offers its artists and albums; an artist
//! is a grid of albums; an album is its track list. Anything else, or a
//! parent whose detail cannot be read, is the plain grid of its children.

use dioxus::prelude::*;
use futures_util::future::{join, join3, try_join};
use futures_util::StreamExt as _;
use pharos_client::api::{
    ClientEvent, ClientEventKind, ClientLevel, Detail, Item, ItemKind, ItemRef, LibraryKind,
    ListIndex, Listing, Page, PharosApi,
};
use pharos_client::{CallInfo, ClientError};

use super::app::{call, remount_on, Ctx};
use super::letters::{self, Wheel, LETTERS};
use super::machine::Event;
use super::paging::{Pager, Trigger};
use super::platform;
use super::route::Route;
use super::views::{progress_percent, runtime, ArtPlaceholder, Link, PosterTile, Ratings};

/// Most children one browse page lists.
pub const BROWSE_PAGE: u32 = 100;

/// `SortBy` for a season's episodes or an album's tracks: season (disc),
/// then episode (track) number.
const INDEX_ORDER: &str = "ParentIndexNumber,IndexNumber";

/// What a browse screen shows under a title that could not be fetched.
pub const BROWSE_TITLE_FALLBACK: &str = "Browse";

/// What `ui_action` a further page of a listing is fetched under.
pub const MORE_ACTION: &str = "more";

/// Most tiles one page holds when a jump to a letter reads ahead; the
/// server's own cap on a native page.
pub const JUMP_PAGE: u32 = 1000;

/// What a library's filter bar orders by (B312/V252).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Order {
    Name,
    /// Release date, then year for anything without one.
    Released,
    /// When the item reached the library; a series, its newest episode.
    Added,
}

impl Order {
    pub const ALL: [Order; 3] = [Order::Name, Order::Released, Order::Added];

    pub fn label(self) -> &'static str {
        match self {
            Order::Name => "Name",
            Order::Released => "Release date",
            Order::Added => "Date added",
        }
    }

    /// The `<option>` value naming it.
    pub fn token(self) -> &'static str {
        match self {
            Order::Name => "name",
            Order::Released => "released",
            Order::Added => "added",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.token() == token)
    }

    /// The `SortBy` it asks for; `None` keeps the listing's own name order.
    fn sort_by(self) -> Option<&'static str> {
        match self {
            Order::Name => None,
            Order::Released => Some("PremiereDate,ProductionYear,SortName"),
            Order::Added => Some("DateCreated,SortName"),
        }
    }

    /// The direction it starts in: A to Z, or newest first.
    pub fn starts_descending(self) -> bool {
        self != Order::Name
    }
}

/// How a library's filter bar narrows and orders its grid. The default is
/// the listing as the library opens: everything, by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Narrow {
    pub order: Order,
    pub descending: bool,
    pub genre: Option<String>,
    /// Only watched (`Some(true)`) or unwatched (`Some(false)`) items.
    pub played: Option<bool>,
}

impl Default for Narrow {
    fn default() -> Self {
        Self {
            order: Order::Name,
            descending: false,
            genre: None,
            played: None,
        }
    }
}

impl Narrow {
    /// `base` ordered and narrowed this way.
    pub fn apply(&self, base: &Listing) -> Listing {
        Listing {
            sort_by: self
                .order
                .sort_by()
                .map_or_else(|| base.sort_by.clone(), str::to_owned),
            descending: self.descending,
            genre: self.genre.clone(),
            played: self.played,
            ..base.clone()
        }
    }

    /// Ordered `order`, in the direction it starts in.
    pub fn ordered(&self, order: Order) -> Self {
        Self {
            order,
            descending: order.starts_descending(),
            ..self.clone()
        }
    }

    /// Whether the grid is in name order, so jumping by letter means
    /// something.
    pub fn by_letter(&self) -> bool {
        self.order == Order::Name
    }
}

/// A loaded browse screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Browse {
    /// The parent's children as posters: a movie library, a TV library's
    /// series, a collection.
    Grid {
        title: String,
        page: Page,
        /// How to list the rest of `page`, when one paged listing backs it.
        more: Option<Listing>,
        /// A library's own grid, which gets the filter bar and letter rail.
        library: bool,
    },
    Show(Show),
    Music {
        title: String,
        artists: Page,
        albums: Page,
    },
    Artist {
        detail: Detail,
        albums: Page,
    },
    Album {
        detail: Detail,
        tracks: Page,
    },
}

/// A series page: its detail when it could be read, its seasons, the season
/// shown and that season's episodes.
#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    pub title: String,
    pub series: Option<Detail>,
    pub seasons: Vec<Item>,
    pub selected: Option<ItemRef>,
    pub episodes: Page,
}

/// How a parent's children are fetched and laid out, decided from its
/// detail alone so the choice is testable without a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Stored children, by name.
    Grid,
    /// A TV library: its series, by name.
    SeriesGrid,
    /// A library of mixed kinds: its series, if it has any, ahead of its
    /// other stored items; episodes appear only inside their series.
    MixedLibrary,
    MusicLibrary,
    /// A series: its seasons, the first regular one shown.
    Series,
    /// A season: its series' page with this season shown.
    Season {
        series: Option<ItemRef>,
    },
    Artist,
    Album,
}

pub fn plan(detail: Option<&Detail>) -> Plan {
    let Some(item) = detail.map(|d| &d.item) else {
        return Plan::Grid;
    };
    match (item.kind, item.library_kind) {
        (ItemKind::Library, Some(LibraryKind::TvShows)) => Plan::SeriesGrid,
        (ItemKind::Library, Some(LibraryKind::Music)) => Plan::MusicLibrary,
        (ItemKind::Library, Some(LibraryKind::Mixed)) => Plan::MixedLibrary,
        (ItemKind::Series, _) => Plan::Series,
        (ItemKind::Season, _) => Plan::Season {
            series: item.parent,
        },
        (ItemKind::Artist, _) => Plan::Artist,
        (ItemKind::Album, _) => Plan::Album,
        _ => Plan::Grid,
    }
}

/// A mixed library's listing: its series, then its stored items other than
/// episodes, which the series tiles stand for. With no series, the stored
/// items as they are.
pub fn series_first(series: Page, stored: Page) -> Page {
    if series.items.is_empty() {
        return stored;
    }
    let episodes = stored
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Episode)
        .count();
    let rest = stored
        .items
        .into_iter()
        .filter(|i| i.kind != ItemKind::Episode);
    let total = series.total
        + stored
            .total
            .saturating_sub(u32::try_from(episodes).unwrap_or(u32::MAX));
    let items: Vec<Item> = series.items.into_iter().chain(rest).collect();
    Page {
        total: total.max(u32::try_from(items.len()).unwrap_or(u32::MAX)),
        items,
        start: 0,
    }
}

/// The season a series page opens on: the first numbered above 0, so
/// specials (season 0) do not hide the show's start; else the first.
pub fn default_season(seasons: &[Item]) -> Option<ItemRef> {
    seasons
        .iter()
        .find(|s| s.episode.is_some_and(|n| n > 0))
        .or(seasons.first())
        .map(|s| s.id)
}

/// Fetches `limit` items of `source` from `start`. Every failure is
/// logged through `warn` and reported through `report` as an `rpcFailed`
/// event, as a success is recorded (V237). `call` builds the `CallInfo`
/// from a route and action; this stamps it `browse` / `more`.
pub async fn fetch_more<A: PharosApi>(
    api: &A,
    call: impl Fn(&str, &str) -> CallInfo,
    source: &Listing,
    start: u32,
    limit: u32,
    warn: impl Fn(String),
    report: impl Fn(ClientEvent),
) -> Result<Page, ClientError> {
    let info = call("browse", MORE_ACTION);
    let result = api.query(&info, source, start, limit).await;
    if let Err(e) = &result {
        let outcome = match e {
            ClientError::Api { kind, .. } => kind,
            ClientError::Rpc(_) => "transport",
        };
        warn(format!(
            "pharos: browse {} more failed start={start} outcome={outcome}: {e}",
            source.parent
        ));
        report(ClientEvent {
            kind: ClientEventKind::RpcFailed,
            level: ClientLevel::Warn,
            route: "browse".to_owned(),
            fields: vec![
                ("interface", "library".to_owned()),
                ("method", "list".to_owned()),
                ("outcome", outcome.to_owned()),
                ("reason", e.to_string()),
            ],
        });
    }
    result
}

/// Loads the browse screen for `id`. The listing is the point of the screen,
/// so a detail that cannot be read is reported through `warn` and the plain
/// grid shown; only a failed listing fails the screen.
pub async fn load<A: PharosApi>(
    api: &A,
    info: &CallInfo,
    id: ItemRef,
    warn: impl Fn(String),
) -> Result<Browse, ClientError> {
    let detail = match api.detail(info, id).await {
        Ok(detail) => Some(detail),
        Err(e) => {
            warn(format!("pharos: browse {id}: title unavailable: {e}"));
            None
        }
    };
    let title = detail
        .as_ref()
        .map_or_else(|| BROWSE_TITLE_FALLBACK.to_owned(), |d| d.item.name.clone());
    let list = |parent, group, sort| api.list(info, parent, group, sort, 0, BROWSE_PAGE);
    let paged = |more: Listing| async move { api.query(info, &more, 0, BROWSE_PAGE).await };
    let library = detail
        .as_ref()
        .is_some_and(|d| d.item.kind == ItemKind::Library);
    Ok(match (plan(detail.as_ref()), detail) {
        (Plan::SeriesGrid, _) => {
            let more = Listing::of(id, Some(ItemKind::Series), "SortName");
            Browse::Grid {
                title,
                page: paged(more.clone()).await?,
                more: Some(more),
                library,
            }
        }
        (Plan::MixedLibrary, _) => {
            let (series, stored) = try_join(
                list(id, Some(ItemKind::Series), "SortName"),
                list(id, None, "SortName"),
            )
            .await?;
            // Two listings merged: no single list to continue.
            Browse::Grid {
                title,
                page: series_first(series, stored),
                more: None,
                library: false,
            }
        }
        (Plan::MusicLibrary, _) => {
            let (artists, albums) = try_join(
                list(id, Some(ItemKind::Artist), "SortName"),
                list(id, Some(ItemKind::Album), "SortName"),
            )
            .await?;
            Browse::Music {
                title,
                artists,
                albums,
            }
        }
        (Plan::Series, series) => {
            let seasons = list(id, Some(ItemKind::Season), "").await?.items;
            let selected = default_season(&seasons);
            let episodes = match selected {
                Some(season) => list(season, None, INDEX_ORDER).await?,
                None => list(id, None, INDEX_ORDER).await?,
            };
            Browse::Show(Show {
                title,
                series,
                seasons,
                selected,
                episodes,
            })
        }
        (
            Plan::Season {
                series: Some(series_id),
            },
            season,
        ) => {
            // The series' detail, its seasons and this season's episodes do
            // not depend on each other: one round trip, not three.
            let (series, seasons, episodes) = join3(
                api.detail(info, series_id),
                list(series_id, Some(ItemKind::Season), ""),
                list(id, None, INDEX_ORDER),
            )
            .await;
            let series = match series {
                Ok(series) => Some(series),
                Err(e) => {
                    warn(format!(
                        "pharos: browse {id}: series {series_id} unavailable: {e}"
                    ));
                    None
                }
            };
            let title = series.as_ref().map_or_else(
                || {
                    season
                        .and_then(|s| s.item.series_name)
                        .unwrap_or_else(|| title.clone())
                },
                |s| s.item.name.clone(),
            );
            Browse::Show(Show {
                title,
                series,
                seasons: seasons?.items,
                selected: Some(id),
                episodes: episodes?,
            })
        }
        (Plan::Artist, Some(detail)) => Browse::Artist {
            albums: list(id, Some(ItemKind::Album), "ProductionYear,SortName").await?,
            detail,
        },
        (Plan::Album, Some(detail)) => Browse::Album {
            tracks: list(id, None, INDEX_ORDER).await?,
            detail,
        },
        (Plan::Season { series: None }, _) => {
            let more = Listing::of(id, None, INDEX_ORDER);
            Browse::Grid {
                title,
                page: paged(more.clone()).await?,
                more: Some(more),
                library: false,
            }
        }
        _ => {
            let more = Listing::of(id, None, "SortName");
            Browse::Grid {
                title,
                page: paged(more.clone()).await?,
                more: Some(more),
                library,
            }
        }
    })
}

#[component]
pub fn BrowseLayout(browse: Browse) -> Element {
    match browse {
        Browse::Grid {
            title,
            page,
            more,
            library,
        } => rsx! { GridView { title, page, more, library } },
        Browse::Show(show) => rsx! { ShowView { show } },
        Browse::Music {
            title,
            artists,
            albums,
        } => rsx! { MusicLibraryView { title, artists, albums } },
        Browse::Artist { detail, albums } => rsx! { ArtistView { detail, albums } },
        Browse::Album { detail, tracks } => rsx! { AlbumView { detail, tracks } },
    }
}

#[component]
pub fn GridView(title: String, page: Page, more: Option<Listing>, library: bool) -> Element {
    rsx! {
        section { class: "pharos-browse",
            h2 { "{title}" }
            match more {
                Some(base) if library => rsx! { LibraryGrid { first: page, base } },
                Some(source) => remount_on(
                    &source.parent.to_string(),
                    rsx! { PagedGrid { first: page, source, by_letter: false } },
                ),
                None => rsx! { PosterGrid { page } },
            }
        }
    }
}

/// The `<option>` value of "every genre" and "watched or not".
const ANY: &str = "";

/// A library's grid under its filter bar (B312/V252): the bar's choice is
/// the listing, and a new choice starts the grid afresh from its first page.
#[component]
fn LibraryGrid(first: Page, base: Listing) -> Element {
    let ctx = use_context::<Ctx>();
    let narrow = use_signal(Narrow::default);
    let genres = use_signal(Vec::<String>::new);
    let opening = base.clone();
    // The library opened on its default listing; any other is fetched here.
    let listed = use_resource(move || {
        let listing = narrow.read().apply(&opening);
        let first = first.clone();
        let fresh = *narrow.read() != Narrow::default();
        async move {
            if !fresh {
                return Ok((listing, first));
            }
            let Some(api) = ctx.api.peek().clone() else {
                return Err("not signed in".to_owned());
            };
            fetch_more(
                &api,
                call,
                &listing,
                0,
                BROWSE_PAGE,
                |line| platform::warn(&line),
                platform::report_event,
            )
            .await
            .map(|page| (listing, page))
            .map_err(|e| e.to_string())
        }
    });
    let by_letter = narrow.read().by_letter();
    rsx! {
        FilterBar { narrow, genres }
        match &*listed.read() {
            None => rsx! { p { class: "pharos-muted", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "pharos-error", role: "alert", "Couldn't load this library: {e}" } },
            Some(Ok((listing, page))) => remount_on(
                &format!("{listing:?}"),
                rsx! {
                    PagedGrid {
                        first: page.clone(),
                        source: listing.clone(),
                        by_letter,
                        genres,
                    }
                },
            ),
        }
    }
}

/// Sort, direction, genre and watched state for a library's grid.
#[component]
fn FilterBar(narrow: Signal<Narrow>, genres: Signal<Vec<String>>) -> Element {
    let now = narrow.read().clone();
    let genre = now.genre.clone().unwrap_or_default();
    let played = match now.played {
        None => ANY,
        Some(false) => "unwatched",
        Some(true) => "watched",
    };
    let direction = match (now.order, now.descending) {
        (Order::Name, false) => "A to Z",
        (Order::Name, true) => "Z to A",
        (_, true) => "Newest first",
        (_, false) => "Oldest first",
    };
    let mut narrow = narrow;
    rsx! {
        div { class: "pharos-filters", role: "group", aria_label: "Sort and filter",
            label { class: "pharos-filter",
                span { aria_hidden: "true", "Sort" }
                select {
                    aria_label: "Sort",
                    value: now.order.token(),
                    onchange: move |e| {
                        if let Some(order) = Order::from_token(&e.value()) {
                            let next = narrow.read().ordered(order);
                            narrow.set(next);
                        }
                    },
                    for order in Order::ALL {
                        option { value: order.token(), selected: order == now.order, "{order.label()}" }
                    }
                }
            }
            button {
                class: "pharos-button pharos-filter-direction",
                r#type: "button",
                aria_label: "Sort direction: {direction}",
                onclick: move |_| narrow.with_mut(|n| n.descending = !n.descending),
                "{direction}"
            }
            label { class: "pharos-filter",
                span { aria_hidden: "true", "Genre" }
                select {
                    aria_label: "Genre",
                    value: "{genre}",
                    onchange: move |e| {
                        let value = e.value();
                        narrow.with_mut(|n| n.genre = (value != ANY).then_some(value));
                    },
                    option { value: ANY, selected: genre.is_empty(), "All genres" }
                    for g in genres.read().iter().cloned() {
                        option { value: "{g}", selected: g == genre, "{g}" }
                    }
                    // A chosen genre stays offered while the list reloads.
                    if !genre.is_empty() && !genres.read().contains(&genre) {
                        option { value: "{genre}", selected: true, "{genre}" }
                    }
                }
            }
            label { class: "pharos-filter",
                span { aria_hidden: "true", "Watched" }
                select {
                    aria_label: "Watched",
                    value: played,
                    onchange: move |e| {
                        let value = e.value();
                        narrow.with_mut(|n| {
                            n.played = match value.as_str() {
                                "watched" => Some(true),
                                "unwatched" => Some(false),
                                _ => None,
                            }
                        });
                    },
                    option { value: ANY, selected: played == ANY, "All" }
                    option { value: "unwatched", selected: played == "unwatched", "Unwatched" }
                    option { value: "watched", selected: played == "watched", "Watched" }
                }
            }
        }
    }
}

/// The `id` of the tile the scroll sentinel watches.
const SENTINEL_ID: &str = "pharos-more-sentinel";

/// The `id` of the marker after the last tile. A jump to the bottom (End,
/// a fling) passes the lead tile without it ever being in view, so the
/// observer watches this too.
const END_ID: &str = "pharos-more-end";

/// What a grid asks its pager for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// The next page, as the viewer nears the end or presses the button.
    Next(Trigger),
    /// Every page through the tile at this offset, for a jump to a letter.
    Through(u32),
}

/// A grid that loads the rest of its listing as the viewer nears the end
/// (V237): an observer on a tile `SENTINEL_LEAD` before the last, and on
/// the end of the list, asks for the next page, and a button does the same by
/// hand and retries a failure (V237; B294 was the dead end this replaces).
/// In name order it also jumps by letter (V252): the rail on the right goes
/// straight to one, and Shift+wheel steps to the next or previous, reading
/// ahead through the pages in between.
#[component]
fn PagedGrid(
    first: Page,
    source: Listing,
    by_letter: bool,
    genres: Option<Signal<Vec<String>>>,
) -> Element {
    let ctx = use_context::<Ctx>();
    let mut pager = use_signal(|| Pager::new(first.clone()));
    // The observer and the button fire outside any one render, so both send
    // here; `Pager::begin` is what keeps a second request out while one is
    // in flight.
    let paged = source.clone();
    let requests = use_coroutine(move |mut rx: UnboundedReceiver<Ask>| {
        let paged = paged.clone();
        async move {
            while let Some(ask) = rx.next().await {
                let claimed = match ask {
                    Ask::Next(trigger) => pager.write().begin(trigger).map(|s| (s, BROWSE_PAGE)),
                    Ask::Through(target) => {
                        pager.write().begin_through(target, BROWSE_PAGE, JUMP_PAGE)
                    }
                };
                let Some((start, limit)) = claimed else {
                    continue;
                };
                let Some(api) = ctx.api.peek().clone() else {
                    pager.write().fail();
                    continue;
                };
                let source = paged.clone();
                spawn(async move {
                    let result = fetch_more(
                        &api,
                        call,
                        &source,
                        start,
                        limit,
                        |line| platform::warn(&line),
                        platform::report_event,
                    )
                    .await;
                    match result {
                        Ok(page) => pager.write().land(page),
                        Err(e) => {
                            if e.needs_sign_in() {
                                let mut machine = ctx.machine;
                                machine.write().apply(Event::NeedsSignIn);
                            }
                            pager.write().fail();
                        }
                    }
                });
            }
        }
    });
    let mut watch = use_signal(|| None::<platform::SentinelWatch>);
    // Runs once the tiles are in the page, and again after every change to
    // the pager so the observer is re-armed on the tile that is now
    // `SENTINEL_LEAD` from the end (it reports a tile already in view).
    use_effect(move || {
        let armed = pager.read().sentinel().is_some();
        watch.set(if armed {
            platform::watch_sentinel(&[SENTINEL_ID, END_ID], move || {
                requests.send(Ask::Next(Trigger::Scroll))
            })
        } else {
            None
        });
    });

    // Where each letter starts, and the genres for the filter bar.
    let indexed = source.clone();
    let index = use_resource(move || {
        let listing = indexed.clone();
        async move {
            let api = ctx.api.peek().clone()?;
            match api.index(&call("browse", "index"), &listing).await {
                Ok(index) => Some(index),
                Err(e) => {
                    platform::warn(&format!(
                        "pharos: browse {} index failed: {e}",
                        listing.parent
                    ));
                    None
                }
            }
        }
    });
    use_effect(move || {
        if let (Some(Some(index)), Some(mut genres)) = (&*index.read(), genres) {
            if *genres.peek() != index.genres {
                genres.set(index.genres.clone());
            }
        }
    });
    let starts = use_memo(move || match &*index.read() {
        Some(Some(ListIndex { letters, .. })) => letters.clone(),
        _ => Vec::new(),
    });

    // A jump: the tile to bring to the top, once its page is in, and the
    // tile last jumped to, which Shift+wheel steps on from while in view.
    let mut jump = use_signal(|| None::<u32>);
    let mut cursor = use_signal(|| None::<u32>);
    use_effect(move || {
        let Some(target) = jump() else {
            return;
        };
        let state = pager.read();
        let loaded = u32::try_from(state.items().len()).unwrap_or(u32::MAX);
        if target < loaded {
            platform::scroll_to_tile(target);
            jump.set(None);
        } else if state.failed() || !state.has_more() {
            jump.set(None);
        } else if !state.in_flight() {
            requests.send(Ask::Through(target));
        }
    });
    let mut go = move |target: u32| {
        cursor.set(Some(target));
        jump.set(Some(target));
    };
    let mut wheel = use_signal(Wheel::default);
    use_hook(move || {
        by_letter.then(|| {
            std::rc::Rc::new(platform::on_shift_wheel(move |turn| {
                let steps = wheel.write().feed(turn);
                if steps == 0 {
                    return;
                }
                let here = cursor
                    .peek()
                    .filter(|&c| platform::tile_in_view(c))
                    .or_else(platform::first_visible_tile)
                    .unwrap_or(0);
                let starts = starts.peek();
                let mut at = here;
                for _ in 0..steps.unsigned_abs() {
                    match letters::step(&starts, at, steps > 0) {
                        Some(next) => at = next,
                        None => break,
                    }
                }
                if at != here {
                    go(at);
                }
            }))
        })
    });

    let state = pager.read();
    let page = Page {
        items: state.items().to_vec(),
        total: state.total(),
        start: 0,
    };
    let label = if state.in_flight() {
        "Loading…"
    } else if state.failed() {
        "Try again"
    } else {
        "Show more"
    };
    let starts = starts.read().clone();
    let current = cursor().and_then(|at| letters::letter_at(&starts, at));
    rsx! {
        if by_letter && !starts.is_empty() {
            nav { class: "pharos-letters", aria_label: "Jump to letter",
                for letter in LETTERS {
                    {
                        let start = letters::start_of(&starts, letter);
                        rsx! {
                            button {
                                class: "pharos-letter",
                                r#type: "button",
                                disabled: start.is_none(),
                                "aria-current": if current == Some(letter) { "true" },
                                aria_label: if letter == '#' { "Numbers and symbols" },
                                onclick: move |_| {
                                    if let Some(start) = start {
                                        go(start);
                                    }
                                },
                                "{letter}"
                            }
                        }
                    }
                }
            }
        }
        PosterList { page, sentinel: state.sentinel() }
        if state.has_more() {
            div { id: END_ID, aria_hidden: "true" }
            p { class: "pharos-muted", "Showing {state.items().len()} of {state.total()}." }
            button {
                class: "pharos-button",
                r#type: "button",
                disabled: state.in_flight(),
                onclick: move |_| requests.send(Ask::Next(Trigger::Button)),
                "{label}"
            }
        }
    }
}

/// Posters for a page, with the empty and "more than shown" notes.
#[component]
fn PosterGrid(page: Page, square: Option<bool>) -> Element {
    rsx! {
        PosterList { page: page.clone(), square }
        MoreNote { page }
    }
}

/// The tiles of a page; the one at `sentinel` carries [`SENTINEL_ID`].
#[component]
fn PosterList(page: Page, square: Option<bool>, sentinel: Option<usize>) -> Element {
    let class = match square {
        Some(true) => "pharos-grid pharos-grid-square",
        _ => "pharos-grid",
    };
    rsx! {
        if page.items.is_empty() {
            p { class: "pharos-empty", "Nothing in here yet." }
        }
        ul { class,
            for (at, item) in page.items.iter().enumerate() {
                li {
                    key: "{item.id}",
                    id: if sentinel == Some(at) { SENTINEL_ID },
                    PosterTile { item: item.clone() }
                }
            }
        }
    }
}

#[component]
fn MoreNote(page: Page) -> Element {
    rsx! {
        if page.total as usize > page.items.len() {
            p { class: "pharos-muted", "Showing {page.items.len()} of {page.total}." }
        }
    }
}

/// A hero for a series, artist or album: art, name, facts and overview.
#[component]
fn Hero(title: String, detail: Option<Detail>, facts: Vec<String>, square: bool) -> Element {
    let art_class = if square {
        "pharos-hero-art pharos-hero-art-square"
    } else {
        "pharos-hero-art"
    };
    let image = detail.as_ref().and_then(|d| d.item.image.clone());
    let kind = detail.as_ref().map_or(ItemKind::Series, |d| d.item.kind);
    let facts = facts.join(" · ");
    rsx! {
        header { class: "pharos-hero",
            div { class: art_class,
                match image {
                    Some(src) => rsx! { img { src: "{src}", alt: "" } },
                    None => rsx! { ArtPlaceholder { kind, name: title.clone() } },
                }
            }
            div { class: "pharos-hero-body",
                h2 { "{title}" }
                if !facts.is_empty() {
                    p { class: "pharos-muted", "{facts}" }
                }
                if let Some(d) = detail.as_ref() {
                    Ratings { kind: d.item.kind, imdb: d.item.imdb_rating, critic: d.item.critic_rating, tile: false }
                }
                if let Some(d) = detail.as_ref().filter(|d| !d.overview.is_empty()) {
                    p { class: "pharos-overview", "{d.overview}" }
                }
                if let Some(d) = detail.as_ref().filter(|d| !d.genres.is_empty()) {
                    p { class: "pharos-muted", "{d.genres.join(\", \")}" }
                }
            }
        }
    }
}

/// The show page: the series hero, its seasons as tabs (links, so the
/// season shown is in the URL), and the season's episodes.
#[component]
pub fn ShowView(show: Show) -> Element {
    let facts: Vec<String> = [
        show.series
            .as_ref()
            .and_then(|d| d.item.year)
            .map(|y| y.to_string()),
        match show.seasons.len() {
            0 => None,
            1 => Some("1 season".to_owned()),
            n => Some(format!("{n} seasons")),
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    rsx! {
        section { class: "pharos-show",
            Hero { title: show.title.clone(), detail: show.series.clone(), facts, square: false }
            if show.seasons.len() > 1 {
                nav { class: "pharos-tabs", aria_label: "Seasons",
                    for season in show.seasons.iter() {
                        Link {
                            key: "{season.id}",
                            to: Route::Browse(season.id),
                            class: if Some(season.id) == show.selected { "pharos-tab pharos-tab-active" } else { "pharos-tab" },
                            "{season.name}"
                        }
                    }
                }
            } else if let Some(season) = show.seasons.first() {
                h3 { class: "pharos-section-title", "{season.name}" }
            }
            EpisodeList { page: show.episodes.clone() }
        }
    }
}

/// A season's episodes as rows: still, number and name, runtime, watched
/// state and progress. The row opens the episode; Play starts it.
#[component]
pub fn EpisodeList(page: Page) -> Element {
    rsx! {
        if page.items.is_empty() {
            p { class: "pharos-empty", "No episodes yet." }
        }
        ol { class: "pharos-episodes",
            for ep in page.items.iter() {
                li { key: "{ep.id}", EpisodeRow { episode: ep.clone() } }
            }
        }
        MoreNote { page }
    }
}

#[component]
fn EpisodeRow(episode: Item) -> Element {
    let progress = progress_percent(&episode);
    let label = match episode.episode {
        Some(n) => format!("{n}. {}", episode.name),
        None => episode.name.clone(),
    };
    let length = runtime(episode.runtime_ms);
    rsx! {
        div { class: "pharos-episode",
            Link { to: Route::Title(episode.id), class: "pharos-episode-link",
                div { class: "pharos-episode-still",
                    if let Some(src) = &episode.image {
                        img { src: "{src}", alt: "", loading: "lazy", decoding: "async" }
                    }
                    if let Some(p) = progress {
                        div { class: "pharos-progress", style: "width: {p}%" }
                    }
                }
                div { class: "pharos-episode-text",
                    span { class: "pharos-episode-name", "{label}" }
                    span { class: "pharos-muted",
                        if let Some(l) = length { "{l}" }
                        if episode.played { " · Watched" }
                    }
                }
            }
            Link { to: Route::Watch(episode.id), class: "pharos-button pharos-episode-play",
                if progress.is_some() { "Resume" } else { "Play" }
            }
        }
    }
}

/// Which list a music library shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MusicTab {
    Artists,
    Albums,
}

/// A music library: its artists or its albums, switched by tabs.
#[component]
pub fn MusicLibraryView(title: String, artists: Page, albums: Page) -> Element {
    let mut tab = use_signal(|| MusicTab::Artists);
    let tab_class = move |t| {
        if tab() == t {
            "pharos-tab pharos-tab-active"
        } else {
            "pharos-tab"
        }
    };
    rsx! {
        section { class: "pharos-browse pharos-music",
            h2 { "{title}" }
            div { class: "pharos-tabs", role: "tablist",
                button {
                    r#type: "button",
                    role: "tab",
                    class: tab_class(MusicTab::Artists),
                    aria_selected: tab() == MusicTab::Artists,
                    onclick: move |_| tab.set(MusicTab::Artists),
                    "Artists"
                }
                button {
                    r#type: "button",
                    role: "tab",
                    class: tab_class(MusicTab::Albums),
                    aria_selected: tab() == MusicTab::Albums,
                    onclick: move |_| tab.set(MusicTab::Albums),
                    "Albums"
                }
            }
            match tab() {
                MusicTab::Artists => rsx! { PosterGrid { page: artists.clone(), square: true } },
                MusicTab::Albums => rsx! { PosterGrid { page: albums.clone(), square: true } },
            }
        }
    }
}

/// An artist: hero, then the albums as square covers.
#[component]
pub fn ArtistView(detail: Detail, albums: Page) -> Element {
    let facts = match albums.total {
        0 => Vec::new(),
        1 => vec!["1 album".to_owned()],
        n => vec![format!("{n} albums")],
    };
    rsx! {
        section { class: "pharos-artist",
            Hero { title: detail.item.name.clone(), detail: Some(detail.clone()), facts, square: true }
            PosterGrid { page: albums, square: true }
        }
    }
}

/// An album: cover and facts, then its tracks, grouped by disc when it has
/// more than one.
#[component]
pub fn AlbumView(detail: Detail, tracks: Page) -> Element {
    let total_ms: u64 = tracks.items.iter().map(|t| t.runtime_ms).sum();
    let facts: Vec<String> = [
        detail.item.year.map(|y| y.to_string()),
        Some(match tracks.total {
            1 => "1 track".to_owned(),
            n => format!("{n} tracks"),
        }),
        runtime(total_ms),
    ]
    .into_iter()
    .flatten()
    .collect();
    let discs = discs(&tracks.items);
    let multi = discs.len() > 1;
    rsx! {
        section { class: "pharos-album",
            Hero { title: detail.item.name.clone(), detail: Some(detail.clone()), facts, square: true }
            if tracks.items.is_empty() {
                p { class: "pharos-empty", "No tracks yet." }
            }
            for (disc, rows) in discs {
                div { key: "{disc}", class: "pharos-disc",
                    if multi {
                        h3 { class: "pharos-section-title", "Disc {disc}" }
                    }
                    ol { class: "pharos-tracks",
                        for track in rows {
                            li { key: "{track.id}", TrackRow { track } }
                        }
                    }
                }
            }
            MoreNote { page: tracks }
        }
    }
}

/// `tracks` split by disc, in the order given; a track with no disc number
/// is on disc 1.
pub fn discs(tracks: &[Item]) -> Vec<(u32, Vec<Item>)> {
    let mut out: Vec<(u32, Vec<Item>)> = Vec::new();
    for t in tracks {
        let disc = t.season.unwrap_or(1);
        match out.last_mut() {
            Some((d, rows)) if *d == disc => rows.push(t.clone()),
            _ => out.push((disc, vec![t.clone()])),
        }
    }
    out
}

/// "3:07"; nothing for a track with no length.
pub fn track_length(ms: u64) -> Option<String> {
    let secs = ms / 1000;
    (secs > 0).then(|| format!("{}:{:02}", secs / 60, secs % 60))
}

#[component]
fn TrackRow(track: Item) -> Element {
    let length = track_length(track.runtime_ms);
    rsx! {
        Link { to: Route::Watch(track.id), class: "pharos-track",
            span { class: "pharos-track-number",
                if let Some(n) = track.episode { "{n}" }
            }
            span { class: "pharos-track-name", "{track.name}" }
            span { class: "pharos-track-length pharos-muted",
                if let Some(l) = length { "{l}" }
            }
        }
    }
}

/// The episodes either side of one, for the player's previous and next.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Neighbours {
    pub previous: Option<ItemRef>,
    pub next: Option<ItemRef>,
}

/// `episode`'s neighbours: the episodes beside it in its season, else the
/// last of the season before or the first of the season after. Crossing a
/// season skips specials (season 0), so season 1 runs on into season 2; a
/// special has neighbours only among the specials. A track's parent is its
/// album, so it gets the tracks beside it; its artist lists no seasons, so
/// it never crosses albums.
pub async fn neighbours<A: PharosApi>(
    api: &A,
    info: &CallInfo,
    episode: &Item,
) -> Result<Neighbours, ClientError> {
    let Some(season) = episode.parent else {
        return Ok(Neighbours::default());
    };
    let episodes = |season| list_all(api, info, season, None, INDEX_ORDER);
    // The season's series is needed only at a season's edge, but asking for
    // it alongside the episodes costs no extra round trip.
    let (here, season_detail) = join(episodes(season), api.detail(info, season)).await;
    let here = here?;
    let Some(at) = here.iter().position(|e| e.id == episode.id) else {
        return Ok(Neighbours::default());
    };
    let mut near = Neighbours {
        previous: at.checked_sub(1).and_then(|i| here.get(i)).map(|e| e.id),
        next: here.get(at + 1).map(|e| e.id),
    };
    if near.previous.is_some() && near.next.is_some() {
        return Ok(near);
    }
    let Some(series) = season_detail?.item.parent else {
        return Ok(near);
    };
    let mut seasons = list_all(api, info, series, Some(ItemKind::Season), "").await?;
    seasons.sort_by_key(|s| s.episode.unwrap_or(u32::MAX));
    let Some(s) = seasons.iter().position(|x| x.id == season) else {
        return Ok(near);
    };
    let regular = |x: &&Item| x.episode != Some(0);
    if seasons.get(s).is_some_and(|x| !regular(&x)) {
        return Ok(near);
    }
    let before = near
        .previous
        .is_none()
        .then(|| seasons.get(..s).and_then(|b| b.iter().rev().find(regular)))
        .flatten();
    let after = near
        .next
        .is_none()
        .then(|| seasons.get(s + 1..).and_then(|a| a.iter().find(regular)))
        .flatten();
    let edge = |season: Option<&Item>| {
        let id = season.map(|s| s.id);
        async move {
            match id {
                Some(id) => episodes(id).await.map(Some),
                None => Ok(None),
            }
        }
    };
    let (before, after) = try_join(edge(before), edge(after)).await?;
    if let Some(before) = before {
        near.previous = before.last().map(|e| e.id);
    }
    if let Some(after) = after {
        near.next = after.first().map(|e| e.id);
    }
    Ok(near)
}

/// Every child of `parent`, page after page: a season can run past one page.
async fn list_all<A: PharosApi>(
    api: &A,
    info: &CallInfo,
    parent: ItemRef,
    group: Option<ItemKind>,
    sort_by: &str,
) -> Result<Vec<Item>, ClientError> {
    let mut items = Vec::new();
    loop {
        let start = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let page = api
            .list(info, parent, group, sort_by, start, BROWSE_PAGE)
            .await?;
        let got = page.items.len();
        items.extend(page.items);
        if got == 0 || u32::try_from(items.len()).unwrap_or(u32::MAX) >= page.total {
            return Ok(items);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::RefCell;
    use std::collections::HashMap;

    use pharos_client::api::{Capabilities, Decision, Home, PlaybackGrant, TrackPicks};

    use super::*;
    use crate::native::views::Nav;

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
            album: None,
            artist: None,
        }
    }

    fn detail(item: Item) -> Detail {
        Detail {
            item,
            overview: String::new(),
            genres: Vec::new(),
            chapters: Vec::new(),
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            trickplay: Vec::new(),
            segments: Vec::new(),
        }
    }

    fn page(items: Vec<Item>) -> Page {
        Page {
            total: items.len() as u32,
            items,
            start: 0,
        }
    }

    struct NoGrant;
    impl PlaybackGrant for NoGrant {
        async fn report(
            &self,
            _: &CallInfo,
            _: u64,
            _: bool,
            _: TrackPicks,
        ) -> Result<(), ClientError> {
            Ok(())
        }
        async fn stop(&self, _: &CallInfo, _: u64) -> Result<(), ClientError> {
            Ok(())
        }
    }

    /// A server with fixed details and listings; records each listing asked
    /// for as (parent, group, sort).
    #[derive(Default)]
    struct Fake {
        details: HashMap<ItemRef, Detail>,
        lists: HashMap<(ItemRef, Option<ItemKind>), Vec<Item>>,
        asked: RefCell<Vec<(ItemRef, Option<ItemKind>, String)>>,
        /// Every listing's route, action, start index and limit.
        stamped: RefCell<Vec<(String, String, u32, u32)>>,
        /// Refuses every listing.
        refuse_lists: bool,
    }

    fn refused() -> ClientError {
        ClientError::Api {
            kind: "notFound",
            reason: "item not found".into(),
            trace_id: String::new(),
        }
    }

    impl PharosApi for Fake {
        type Playing = NoGrant;
        async fn home(&self, _: &CallInfo) -> Result<Home, ClientError> {
            Ok(Home::default())
        }
        async fn query(
            &self,
            info: &CallInfo,
            listing: &Listing,
            start: u32,
            limit: u32,
        ) -> Result<Page, ClientError> {
            let (parent, group) = (listing.parent, listing.group);
            self.asked
                .borrow_mut()
                .push((parent, group, listing.sort_by.clone()));
            self.stamped.borrow_mut().push((
                info.ui_route.clone(),
                info.ui_action.clone(),
                start,
                limit,
            ));
            if self.refuse_lists {
                return Err(refused());
            }
            let all = self
                .lists
                .get(&(parent, group))
                .cloned()
                .unwrap_or_default();
            Ok(Page {
                total: all.len() as u32,
                items: all
                    .into_iter()
                    .skip(start as usize)
                    .take(limit as usize)
                    .collect(),
                start,
            })
        }
        async fn index(&self, _: &CallInfo, _: &Listing) -> Result<ListIndex, ClientError> {
            Ok(ListIndex::default())
        }
        async fn search(&self, _: &CallInfo, _: &str, _: u32) -> Result<Page, ClientError> {
            Ok(Page::default())
        }
        async fn detail(&self, _: &CallInfo, id: ItemRef) -> Result<Detail, ClientError> {
            self.details.get(&id).cloned().ok_or_else(refused)
        }
        async fn play(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: &Capabilities,
            _: TrackPicks,
        ) -> Result<(Decision, NoGrant), ClientError> {
            Err(refused())
        }
    }

    /// Runs a future the fake answers without waiting.
    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the fake server never waits"),
        }
    }

    const SERIES: ItemRef = ItemRef::Synth(10);
    const SPECIALS: ItemRef = ItemRef::Synth(11);
    const S1: ItemRef = ItemRef::Synth(12);
    const S2: ItemRef = ItemRef::Synth(13);

    fn show_fake() -> Fake {
        let mut fake = Fake::default();
        fake.details
            .insert(SERIES, detail(item(SERIES, ItemKind::Series, "Show")));
        let mut s2 = item(S2, ItemKind::Season, "Season 2");
        s2.parent = Some(SERIES);
        s2.series_name = Some("Show".into());
        fake.details.insert(S2, detail(s2));
        let season = |id, n, name| {
            let mut s = item(id, ItemKind::Season, name);
            s.episode = Some(n);
            s
        };
        fake.lists.insert(
            (SERIES, Some(ItemKind::Season)),
            vec![
                season(SPECIALS, 0, "Specials"),
                season(S1, 1, "Season 1"),
                season(S2, 2, "Season 2"),
            ],
        );
        fake.lists.insert(
            (S1, None),
            vec![item(ItemRef::Real(1), ItemKind::Episode, "Pilot")],
        );
        fake.lists.insert(
            (S2, None),
            vec![item(ItemRef::Real(2), ItemKind::Episode, "Return")],
        );
        fake
    }

    #[test]
    fn a_series_opens_on_its_first_regular_season_in_episode_order() {
        let fake = show_fake();
        let Browse::Show(show) = run(load(&fake, &CallInfo::default(), SERIES, |_| {})).unwrap()
        else {
            panic!("not a show page");
        };
        assert_eq!(show.title, "Show");
        assert_eq!(show.selected, Some(S1));
        assert_eq!(show.seasons.len(), 3);
        assert_eq!(show.episodes.items[0].name, "Pilot");
        assert!(fake
            .asked
            .borrow()
            .contains(&(S1, None, INDEX_ORDER.to_owned())));
    }

    #[test]
    fn a_season_opens_its_series_page_with_that_season_shown() {
        let fake = show_fake();
        let Browse::Show(show) = run(load(&fake, &CallInfo::default(), S2, |_| {})).unwrap() else {
            panic!("not a show page");
        };
        assert_eq!(show.title, "Show");
        assert!(show.series.is_some());
        assert_eq!(show.selected, Some(S2));
        assert_eq!(show.seasons.len(), 3);
        assert_eq!(show.episodes.items[0].name, "Return");
    }

    #[test]
    fn a_browse_screen_without_a_title_still_shows_its_listing() {
        let mut fake = Fake::default();
        let id = ItemRef::Synth(7);
        fake.lists.insert(
            (id, None),
            vec![item(ItemRef::Real(3), ItemKind::Movie, "Film")],
        );
        let warned = RefCell::new(Vec::new());
        let browse = run(load(&fake, &CallInfo::default(), id, |w| {
            warned.borrow_mut().push(w)
        }))
        .unwrap();
        assert_eq!(
            browse,
            Browse::Grid {
                title: BROWSE_TITLE_FALLBACK.into(),
                page: page(vec![item(ItemRef::Real(3), ItemKind::Movie, "Film")]),
                more: Some(Listing::of(id, None, "SortName")),
                library: false,
            }
        );
        assert!(
            warned.borrow()[0].contains("title unavailable"),
            "{:?}",
            warned.borrow()
        );
    }

    #[test]
    fn libraries_list_by_what_they_hold() {
        let library = |kind| {
            let mut i = item(ItemRef::Synth(1), ItemKind::Library, "L");
            i.library_kind = Some(kind);
            detail(i)
        };
        assert_eq!(plan(Some(&library(LibraryKind::TvShows))), Plan::SeriesGrid);
        assert_eq!(plan(Some(&library(LibraryKind::Music))), Plan::MusicLibrary);
        assert_eq!(plan(Some(&library(LibraryKind::Movies))), Plan::Grid);
        assert_eq!(plan(Some(&library(LibraryKind::Mixed))), Plan::MixedLibrary);
        assert_eq!(plan(None), Plan::Grid);
        let artist = detail(item(ItemRef::Synth(2), ItemKind::Artist, "A"));
        assert_eq!(plan(Some(&artist)), Plan::Artist);
    }

    #[test]
    fn a_mixed_library_shows_series_in_place_of_their_episodes() {
        let series = page(vec![item(SERIES, ItemKind::Series, "Show")]);
        let stored = page(vec![
            item(ItemRef::Real(1), ItemKind::Episode, "Pilot"),
            item(ItemRef::Real(2), ItemKind::Movie, "Film"),
        ]);
        let mixed = series_first(series, stored.clone());
        let names: Vec<_> = mixed.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Show", "Film"]);
        assert_eq!(mixed.total, 2);
        assert_eq!(series_first(Page::default(), stored.clone()), stored);
    }

    #[test]
    fn an_album_lists_its_tracks_by_disc_with_lengths() {
        let track = |id, disc, n, name: &str, ms| {
            let mut t = item(ItemRef::Real(id), ItemKind::Audio, name);
            t.season = Some(disc);
            t.episode = Some(n);
            t.runtime_ms = ms;
            t
        };
        let tracks = vec![
            track(1, 1, 1, "Opener", 187_000),
            track(2, 1, 2, "Second", 61_000),
            track(3, 2, 1, "Disc two", 5_000),
        ];
        assert_eq!(
            discs(&tracks)
                .iter()
                .map(|(d, t)| (*d, t.len()))
                .collect::<Vec<_>>(),
            [(1, 2), (2, 1)]
        );
        let album = detail(item(ItemRef::Synth(5), ItemKind::Album, "Record"));
        let html = render(move || {
            rsx! { AlbumView { detail: album.clone(), tracks: page(tracks.clone()) } }
        });
        assert!(html.contains("Disc 2"), "{html}");
        assert!(html.contains("3:07"), "{html}");
        assert!(html.contains("3 tracks"), "{html}");
        assert!(html.contains("/ui/watch/1"), "{html}");
    }

    #[test]
    fn a_show_page_marks_its_season_and_shows_episode_progress() {
        let mut ep = item(ItemRef::Real(1), ItemKind::Episode, "Pilot");
        ep.episode = Some(1);
        ep.runtime_ms = 60_000;
        ep.position_ms = 30_000;
        let show = Show {
            title: "Show".into(),
            series: None,
            seasons: vec![
                item(S1, ItemKind::Season, "Season 1"),
                item(S2, ItemKind::Season, "Season 2"),
            ],
            selected: Some(S2),
            episodes: page(vec![ep]),
        };
        let html = render(move || rsx! { ShowView { show: show.clone() } });
        assert!(
            html.contains("pharos-tab pharos-tab-active\" href=\"/ui/browse/s13\"")
                || html.contains("href=\"/ui/browse/s13\" class=\"pharos-tab pharos-tab-active\""),
            "{html}"
        );
        assert!(html.contains("1. Pilot"), "{html}");
        assert!(html.contains("width: 50%"), "{html}");
        assert!(html.contains("Resume"), "{html}");
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
    fn stamp(route: &str, action: &str) -> CallInfo {
        CallInfo {
            ui_route: route.into(),
            ui_action: action.into(),
            ..CallInfo::default()
        }
    }

    const BULK: ItemRef = ItemRef::Synth(20);

    fn bulk_source() -> Listing {
        Listing::of(BULK, None, "SortName")
    }

    #[test]
    fn a_further_page_is_asked_for_under_the_more_action() {
        let fake = Fake::default();
        let events = RefCell::new(Vec::new());
        run(fetch_more(
            &fake,
            stamp,
            &bulk_source(),
            100,
            BROWSE_PAGE,
            |_| {},
            |e| events.borrow_mut().push(e),
        ))
        .unwrap();
        assert_eq!(
            *fake.stamped.borrow(),
            [("browse".into(), "more".into(), 100, BROWSE_PAGE)]
        );
        assert!(events.borrow().is_empty(), "a success reports no failure");
    }

    #[test]
    fn a_failed_page_warns_and_reports_an_rpc_failed_event() {
        let fake = Fake {
            refuse_lists: true,
            ..Fake::default()
        };
        let warned = RefCell::new(Vec::new());
        let events = RefCell::new(Vec::new());
        let result = run(fetch_more(
            &fake,
            stamp,
            &bulk_source(),
            200,
            BROWSE_PAGE,
            |l| warned.borrow_mut().push(l),
            |e| events.borrow_mut().push(e),
        ));
        assert!(result.is_err());
        let warned = warned.borrow();
        assert_eq!(warned.len(), 1);
        assert!(warned[0].contains("start=200"), "{}", warned[0]);
        assert!(warned[0].contains("notFound"), "{}", warned[0]);
        let events = events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ClientEventKind::RpcFailed);
        assert_eq!(events[0].route, "browse");
        assert!(events[0]
            .fields
            .contains(&("outcome", "notFound".to_owned())));
        assert!(events[0].fields.contains(&("method", "list".to_owned())));
    }

    #[test]
    fn the_filter_bar_orders_and_narrows_the_listing() {
        let base = Listing::of(ItemRef::Real(4), Some(ItemKind::Series), "SortName");
        assert_eq!(Narrow::default().apply(&base), base, "opens as listed");
        let newest = Narrow::default().ordered(Order::Added);
        let listing = newest.apply(&base);
        assert_eq!(listing.sort_by, "DateCreated,SortName");
        assert!(listing.descending, "a date order starts newest first");
        assert_eq!(listing.group, Some(ItemKind::Series));
        let released = Narrow {
            genre: Some("Drama".into()),
            played: Some(false),
            ..Narrow::default().ordered(Order::Released)
        };
        let listing = released.apply(&base);
        assert_eq!(listing.sort_by, "PremiereDate,ProductionYear,SortName");
        assert_eq!(listing.genre.as_deref(), Some("Drama"));
        assert_eq!(listing.played, Some(false));
        let by_name = released.ordered(Order::Name);
        assert!(by_name.by_letter() && !by_name.descending);
        assert_eq!(by_name.apply(&base).sort_by, "SortName");
        assert_eq!(
            by_name.genre.as_deref(),
            Some("Drama"),
            "a new order keeps the filters"
        );
        assert!(!newest.by_letter());
    }

    #[test]
    fn order_tokens_round_trip() {
        for order in Order::ALL {
            assert_eq!(Order::from_token(order.token()), Some(order));
        }
        assert_eq!(Order::from_token("bogus"), None);
    }

    #[test]
    fn only_a_library_grid_gets_the_filter_bar() {
        let mut fake = Fake::default();
        let lib = ItemRef::Real(9);
        let mut library = item(lib, ItemKind::Library, "Films");
        library.library_kind = Some(LibraryKind::Movies);
        fake.details.insert(lib, detail(library));
        let collection = ItemRef::Real(10);
        fake.details.insert(
            collection,
            detail(item(collection, ItemKind::Collection, "Boxset")),
        );
        let opened = |id| run(load(&fake, &CallInfo::default(), id, |_| {})).unwrap();
        assert!(matches!(opened(lib), Browse::Grid { library: true, .. }));
        assert!(matches!(
            opened(collection),
            Browse::Grid { library: false, .. }
        ));
    }

    fn episode_in(id: ItemRef, season: ItemRef) -> Item {
        let mut e = item(id, ItemKind::Episode, "Episode");
        e.parent = Some(season);
        e
    }

    /// Season 1 of two episodes, season 2 of one, and a special.
    fn run_fake() -> Fake {
        let mut fake = show_fake();
        let mut s1 = item(S1, ItemKind::Season, "Season 1");
        s1.parent = Some(SERIES);
        fake.details.insert(S1, detail(s1));
        let mut specials = item(SPECIALS, ItemKind::Season, "Specials");
        specials.parent = Some(SERIES);
        fake.details.insert(SPECIALS, detail(specials));
        fake.lists.insert(
            (S1, None),
            vec![
                episode_in(ItemRef::Real(1), S1),
                episode_in(ItemRef::Real(4), S1),
            ],
        );
        fake.lists
            .insert((S2, None), vec![episode_in(ItemRef::Real(2), S2)]);
        fake.lists.insert(
            (SPECIALS, None),
            vec![episode_in(ItemRef::Real(9), SPECIALS)],
        );
        fake
    }

    fn near(fake: &Fake, id: ItemRef, season: ItemRef) -> Neighbours {
        run(neighbours(
            fake,
            &CallInfo::default(),
            &episode_in(id, season),
        ))
        .unwrap()
    }

    #[test]
    fn an_episode_inside_its_season_neighbours_its_season() {
        let fake = run_fake();
        assert_eq!(
            near(&fake, ItemRef::Real(1), S1),
            Neighbours {
                previous: None,
                next: Some(ItemRef::Real(4))
            }
        );
    }

    #[test]
    fn the_end_of_a_season_runs_on_into_the_next_skipping_specials() {
        let fake = run_fake();
        assert_eq!(
            near(&fake, ItemRef::Real(4), S1),
            Neighbours {
                previous: Some(ItemRef::Real(1)),
                next: Some(ItemRef::Real(2))
            }
        );
        assert_eq!(
            near(&fake, ItemRef::Real(2), S2),
            Neighbours {
                previous: Some(ItemRef::Real(4)),
                next: None
            }
        );
    }

    #[test]
    fn a_season_longer_than_a_page_is_read_to_its_end() {
        let mut fake = run_fake();
        let long: Vec<Item> = (100..100 + u64::from(BROWSE_PAGE) + 50)
            .map(|n| episode_in(ItemRef::Real(n), S1))
            .collect();
        let last = long.last().map(|e| e.id);
        fake.lists.insert((S1, None), long);
        let beyond = ItemRef::Real(100 + u64::from(BROWSE_PAGE) + 10);
        assert_eq!(
            near(&fake, beyond, S1),
            Neighbours {
                previous: Some(ItemRef::Real(100 + u64::from(BROWSE_PAGE) + 9)),
                next: Some(ItemRef::Real(100 + u64::from(BROWSE_PAGE) + 11)),
            }
        );
        assert_eq!(near(&fake, ItemRef::Real(2), S2).previous, last);
    }

    #[test]
    fn a_special_stays_among_the_specials() {
        let fake = run_fake();
        assert_eq!(
            near(&fake, ItemRef::Real(9), SPECIALS),
            Neighbours::default()
        );
    }

    #[test]
    fn a_track_neighbours_its_album_and_stops_at_its_ends() {
        let mut fake = Fake::default();
        let artist = ItemRef::Synth(30);
        let album = ItemRef::Synth(31);
        let mut a = item(album, ItemKind::Album, "Album");
        a.parent = Some(artist);
        fake.details.insert(album, detail(a));
        let track = |n| {
            let mut t = item(ItemRef::Real(n), ItemKind::Audio, "Track");
            t.parent = Some(album);
            t
        };
        fake.lists.insert((album, None), vec![track(40), track(41)]);
        let near = |n| run(neighbours(&fake, &CallInfo::default(), &track(n))).unwrap();
        assert_eq!(
            near(40),
            Neighbours {
                previous: None,
                next: Some(ItemRef::Real(41)),
            }
        );
        assert_eq!(
            near(41),
            Neighbours {
                previous: Some(ItemRef::Real(40)),
                next: None,
            }
        );
    }

    #[test]
    fn a_film_has_no_neighbours() {
        let fake = Fake::default();
        let film = item(ItemRef::Real(3), ItemKind::Movie, "Film");
        let n = run(neighbours(&fake, &CallInfo::default(), &film)).unwrap();
        assert_eq!(n, Neighbours::default());
    }
}
