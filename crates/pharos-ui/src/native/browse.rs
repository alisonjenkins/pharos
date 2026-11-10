//! The browse screen's layouts (012 US2, US3, US5): what a library, series,
//! season, artist or album shows, chosen from its detail. A TV library is a
//! grid of series; a series or season is the show page, seasons as tabs over
//! an episode list; a music library offers its artists and albums; an artist
//! is a grid of albums; an album is its track list. Anything else, or a
//! parent whose detail cannot be read, is the plain grid of its children.

use std::collections::HashSet;

use dioxus::prelude::*;
use futures_util::future::{join, join3, try_join};
use futures_util::StreamExt as _;
use pharos_client::api::{
    ClientEvent, ClientEventKind, ClientLevel, Detail, Item, ItemKind, ItemRef, LibraryKind,
    ListIndex, Listing, Page, PharosApi, Watched,
};
use pharos_client::{CallInfo, ClientError};

use super::app::{call, remount_on, Ctx};
use super::art::Art;
use super::group_app::Together;
use super::keymap::Mode;
use super::keys::{use_arrival_focus, use_key_mode};
use super::letters::{self, Wheel, LETTERS};
use super::listen::{Icon, ICON_QUEUE_ADD, ICON_QUEUE_NEXT};
use super::machine::Event;
use super::mixes::{self, MixCtx, ICON_MIX};
use super::paging::{Chain, Pager, Trigger, SENTINEL_LEAD};
use super::platform;
use super::queue::{added_line, video_kind, QueueCtx, Track, VideoQueueCtx};
use super::route::Route;
use super::seen::{self, MemberSeen};
use super::together_view::{InThisGroup, SeenDots};
use super::views::{
    display_name, episode_label, is_placeholder_episode_name, progress_percent, runtime,
    ArtPlaceholder, FavoriteToggle, Link, MarkPlayed, PosterTile, Ratings, StoredMark,
    WatchedToggle, MAIN_ACTION,
};
use pharos_client::mix::radio;

/// Most children one browse page lists.
pub const BROWSE_PAGE: u32 = 100;

/// `SortBy` for a season's episodes or an album's tracks: season (disc),
/// then episode (track) number.
const INDEX_ORDER: &str = "ParentIndexNumber,IndexNumber";

/// `SortBy` for an artist's albums: oldest first, then by name.
const ARTIST_ALBUM_ORDER: &str = "ProductionYear,SortName";

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

/// What a library grid's rail jumps between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    /// First letters, `#` then A to Z, in name order (V252).
    Letters,
    /// Years, in the listing's own order, in a date order (B317/V256).
    Years,
}

/// How a library's filter bar narrows and orders its grid. The default is
/// the listing as the library opens: everything, by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Narrow {
    pub order: Order,
    pub descending: bool,
    pub genre: Option<String>,
    /// Only items in this play state.
    pub watched: Option<Watched>,
    /// Only 3D (`Some(true)`) or only 2D (`Some(false)`) titles (V722).
    pub stereo: Option<bool>,
}

impl Default for Narrow {
    fn default() -> Self {
        Self {
            order: Order::Name,
            descending: false,
            genre: None,
            watched: None,
            stereo: None,
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
            watched: self.watched,
            stereo: self.stereo,
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

    /// What the grid's rail jumps between in this order.
    pub fn rail(&self) -> Rail {
        match self.order {
            Order::Name => Rail::Letters,
            Order::Released | Order::Added => Rail::Years,
        }
    }

    /// The URL query naming this choice, `?` included; empty for the
    /// default, so a library as it opens keeps its bare URL (B560).
    pub fn to_query(&self) -> String {
        let mut pairs = Vec::new();
        if self.order != Order::Name {
            pairs.push(format!("sort={}", self.order.token()));
        }
        if self.descending != self.order.starts_descending() {
            pairs.push(format!(
                "dir={}",
                if self.descending { "desc" } else { "asc" }
            ));
        }
        if let Some(genre) = &self.genre {
            pairs.push(format!("genre={}", encode_component(genre)));
        }
        if let Some(watched) = self.watched {
            pairs.push(format!("watched={}", watched_token(watched)));
        }
        match self.stereo {
            Some(true) => pairs.push("3d=3d".to_owned()),
            Some(false) => pairs.push("3d=2d".to_owned()),
            None => {}
        }
        if pairs.is_empty() {
            String::new()
        } else {
            format!("?{}", pairs.join("&"))
        }
    }

    /// The choice a URL query names, `?` optional; a missing or unknown
    /// value keeps the default.
    pub fn from_query(query: &str) -> Self {
        let mut order = Order::Name;
        let mut descending = None;
        let mut genre = None;
        let mut watched = None;
        let mut stereo = None;
        for pair in query.trim_start_matches('?').split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = decode_component(value);
            match (key, value.as_str()) {
                ("sort", v) => order = Order::from_token(v).unwrap_or(Order::Name),
                ("dir", "asc") => descending = Some(false),
                ("dir", "desc") => descending = Some(true),
                ("genre", v) if !v.is_empty() => genre = Some(v.to_owned()),
                ("watched", v) => watched = watched_of(v),
                ("3d", "3d") => stereo = Some(true),
                ("3d", "2d") => stereo = Some(false),
                _ => {}
            }
        }
        Self {
            order,
            descending: descending.unwrap_or(order.starts_descending()),
            genre,
            watched,
            stereo,
        }
    }
}

/// The URL and `<option>` value of a play state.
fn watched_token(watched: Watched) -> &'static str {
    match watched {
        Watched::Unwatched => "unwatched",
        Watched::Watched => "watched",
        Watched::InProgress => "in-progress",
    }
}

/// The play state a URL or `<option>` value names; `None` for any other.
fn watched_of(token: &str) -> Option<Watched> {
    [Watched::Unwatched, Watched::InProgress, Watched::Watched]
        .into_iter()
        .find(|w| watched_token(*w) == token)
}

/// `s` with everything but unreserved URL characters percent-encoded.
pub(crate) fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query value decoded: `+` is a space, `%XX` a byte; a malformed escape
/// is kept as written.
fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b, hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
        /// A Movies library's grid, which offers to group its films into
        /// their box sets (B1086).
        sets: bool,
    },
    Show(Show),
    /// A mixed library: its series, then its other stored items, paged as
    /// one grid (B821).
    Mixed {
        title: String,
        page: Page,
        chain: Chain,
    },
    Music {
        title: String,
        /// The library, whose artists and albums the tabs page through.
        library: ItemRef,
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

impl Browse {
    /// The parent this page lists, for the tab title. `asked` is the item
    /// the route names: a season route reads "<season> · <show>", so each
    /// season's tab has its own title, while the series route (whose shown
    /// season is only the default) reads the show alone.
    pub fn name(&self, asked: ItemRef) -> String {
        match self {
            Browse::Grid { title, .. }
            | Browse::Mixed { title, .. }
            | Browse::Music { title, .. } => title.clone(),
            Browse::Show(show) => match show
                .seasons
                .iter()
                .find(|season| season.id == asked && show.selected == Some(asked))
            {
                Some(season) => format!("{} · {}", season.name, show.title),
                None => show.title.clone(),
            },
            Browse::Artist { detail, .. } | Browse::Album { detail, .. } => {
                detail.item.name.clone()
            }
        }
    }
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
    /// The server's watched state for the shown season as a whole, read
    /// from its detail when `episodes` is only part of it: a page cannot say
    /// whether a longer season is all watched.
    pub season_played: Option<bool>,
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

/// A mixed library's first page: its series, then its stored items, as one
/// listing of `series.total + stored.total` places ([`Chain`]). The stored
/// items join only when every series is already on the page; otherwise the
/// series continue first, and the stored items follow them.
pub fn mixed_first(series: &Page, stored: Page) -> Page {
    let total = series.total.saturating_add(stored.total);
    let whole_head = series.items.len() >= series.total as usize;
    let mut items = series.items.clone();
    if whole_head {
        items.extend(stored.items);
    }
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
    let sets = library
        && detail
            .as_ref()
            .is_some_and(|d| d.item.library_kind == Some(LibraryKind::Movies));
    Ok(match (plan(detail.as_ref()), detail) {
        (Plan::SeriesGrid, _) => {
            let more = Listing::of(id, Some(ItemKind::Series), "SortName");
            Browse::Grid {
                title,
                page: paged(more.clone()).await?,
                more: Some(more),
                library,
                sets: false,
            }
        }
        (Plan::MixedLibrary, _) => {
            let (series, stored) = try_join(
                list(id, Some(ItemKind::Series), "SortName"),
                list(id, None, "SortName"),
            )
            .await?;
            let chain = Chain {
                head: Listing::of(id, Some(ItemKind::Series), "SortName"),
                head_total: series.total,
                tail: Listing::of(id, None, "SortName"),
                // With no series the stored items are all there is, and
                // their episodes show as themselves.
                hide: (series.total > 0).then_some(ItemKind::Episode),
            };
            Browse::Mixed {
                title,
                page: mixed_first(&series, stored),
                chain,
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
                library: id,
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
            let season_played = match selected {
                Some(season) if is_partial(&episodes) => match api.detail(info, season).await {
                    Ok(d) => Some(d.item.played),
                    Err(e) => {
                        warn(format!(
                            "pharos: browse {id}: season {season} watched state unavailable: {e}"
                        ));
                        None
                    }
                },
                _ => None,
            };
            Browse::Show(Show {
                title,
                series,
                seasons,
                selected,
                episodes,
                season_played,
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
            let season_played = season.as_ref().map(|s| s.item.played);
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
                season_played,
            })
        }
        (Plan::Artist, Some(detail)) => Browse::Artist {
            albums: list(id, Some(ItemKind::Album), ARTIST_ALBUM_ORDER).await?,
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
                sets: false,
            }
        }
        _ => {
            let mut more = Listing::of(id, None, "SortName");
            // The library opens as the device last chose, so its first page
            // is fetched once (B1086).
            more.collapse = sets && stored_collections();
            Browse::Grid {
                title,
                page: paged(more.clone()).await?,
                more: Some(more),
                library,
                sets,
            }
        }
    })
}

#[component]
pub fn BrowseLayout(browse: Browse, #[props(default)] seen: Vec<MemberSeen>) -> Element {
    match browse {
        Browse::Grid {
            title,
            page,
            more,
            library,
            sets,
        } => rsx! { GridView { title, page, more, library, sets } },
        Browse::Show(show) => rsx! { ShowView { show, seen } },
        Browse::Mixed { title, page, chain } => rsx! { MixedView { title, page, chain } },
        Browse::Music {
            title,
            library,
            artists,
            albums,
        } => rsx! { MusicLibraryView { title, library, artists, albums } },
        Browse::Artist { detail, albums } => rsx! { ArtistView { detail, albums } },
        Browse::Album { detail, tracks } => rsx! { AlbumView { detail, tracks } },
    }
}

#[component]
pub fn GridView(
    title: String,
    page: Page,
    more: Option<Listing>,
    library: bool,
    /// Offer to group the films into their box sets (B1086).
    #[props(default)]
    sets: bool,
) -> Element {
    rsx! {
        section { class: "pharos-browse",
            h2 { "{title}" }
            match more {
                // The top bar swaps libraries without leaving this view, and
                // the grid's filters and listing belong to the one it opened
                // on, so a new library mounts a new grid (B490).
                Some(base) if library => remount_on(
                    &base.parent.to_string(),
                    rsx! { LibraryGrid { first: page, base, sets } },
                ),
                Some(source) => remount_on(
                    &source.parent.to_string(),
                    rsx! { PagedGrid { first: page, source, rail: None } },
                ),
                None => rsx! { PosterGrid { page } },
            }
        }
    }
}

/// A mixed library's grid: series, then the rest, paged as one (B821).
#[component]
pub fn MixedView(title: String, page: Page, chain: Chain) -> Element {
    rsx! {
        section { class: "pharos-browse",
            h2 { "{title}" }
            {remount_on(
                &chain.head.parent.to_string(),
                rsx! { PagedGrid { first: page, source: chain.head.clone(), rail: None, chain: Some(chain) } },
            )}
        }
    }
}

/// The `<option>` value of "every genre" and "watched or not".
const ANY: &str = "";

/// The `sessionStorage` key of the last filter bar choice for `library`.
fn choice_key(library: ItemRef) -> String {
    format!("pharos.library.{library}.choice")
}

/// The `sessionStorage` key of where the viewer was in `library`'s grid
/// under the choice `query` names.
fn place_key(library: ItemRef, query: &str) -> String {
    format!("pharos.library.{library}.place{query}")
}

/// The choice a library opens on (B560): the one its URL names, so Back
/// and Forward bring it back; else the one last made for it in this tab, so
/// a breadcrumb or library button does too; else the default.
fn opening_choice(library: ItemRef) -> Narrow {
    let search = platform::current_search();
    if Route::parse(&platform::current_path()) == Route::Browse(library) && !search.is_empty() {
        return Narrow::from_query(&search);
    }
    match platform::session_get(&choice_key(library)) {
        Ok(Some(query)) => Narrow::from_query(&query),
        Ok(None) => Narrow::default(),
        Err(e) => {
            platform::warn(&format!("pharos: library {library} filters not read: {e}"));
            Narrow::default()
        }
    }
}

/// Where the viewer was in a grid: the first tile in view, and how far its
/// top sat below the top of the viewport.
fn read_place(key: &str) -> Option<(u32, f64)> {
    let saved = platform::session_get(key).ok().flatten()?;
    let (tile, top) = saved.split_once(' ')?;
    let top: f64 = top.parse().ok()?;
    top.is_finite().then_some((tile.parse().ok()?, top))
}

/// A library's grid under its filter bar (B312/V252): the bar's choice is
/// the listing, and a new choice starts the grid afresh from its first page.
/// The choice is kept in the URL and for the tab, and the grid comes back
/// where it was left while the choice it opened on stands (B560).
#[component]
fn LibraryGrid(first: Page, base: Listing, sets: bool) -> Element {
    let ctx = use_context::<Ctx>();
    // Whether the films are grouped into box sets; the first page was
    // fetched the way the device last chose (B1086).
    let opened_grouped = base.collapse;
    let grouped = use_signal(|| opened_grouped);
    let library = base.parent;
    let opened = use_hook(|| opening_choice(library));
    let narrow = use_signal(|| opened.clone());
    use_effect(move || {
        let query = narrow.read().to_query();
        if let Err(e) = platform::session_set(&choice_key(library), &query) {
            platform::warn(&format!("pharos: library {library} filters not kept: {e}"));
        }
        let path = format!("{}{query}", Route::Browse(library).path());
        let here = Route::parse(&platform::current_path()) == Route::Browse(library);
        if here && platform::current_search() != query {
            platform::replace(&path);
        }
    });
    // A new choice starts at the top, even if it later returns to the one
    // the grid opened on.
    let changed = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    if *narrow.read() != opened {
        changed.set(true);
    }
    let restore = !changed.get();
    let genres = use_signal(Vec::<String>::new);
    let opening = base.clone();
    // The library opened on its default listing; any other is fetched here.
    let listed = use_resource(move || {
        let listing = Listing {
            collapse: *grouped.read(),
            ..narrow.read().apply(&opening)
        };
        let place = place_key(library, &narrow.read().to_query());
        let first = first.clone();
        let fresh = *narrow.read() != Narrow::default() || *grouped.read() != opened_grouped;
        async move {
            if !fresh {
                return Ok((place, listing, first));
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
            .map(|page| (place, listing, page))
            .map_err(|e| e.to_string())
        }
    });
    let rail = narrow.read().rail();
    rsx! {
        FilterBar { narrow, genres, grouped: sets.then_some(grouped) }
        match &*listed.read() {
            None => rsx! { p { class: "pharos-muted", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "pharos-error", role: "alert", "Couldn't load this library: {e}" } },
            Some(Ok((place, listing, page))) => remount_on(
                &format!("{listing:?}"),
                rsx! {
                    PagedGrid {
                        first: page.clone(),
                        source: listing.clone(),
                        rail: Some(rail),
                        genres,
                        place: place.clone(),
                        restore,
                    }
                },
            ),
        }
    }
}

/// Sort, direction, genre and watched state for a library's grid.
#[component]
fn FilterBar(
    narrow: Signal<Narrow>,
    genres: Signal<Vec<String>>,
    /// Whether films are grouped into box sets, on a Movies library (B1086).
    #[props(default)]
    grouped: Option<Signal<bool>>,
) -> Element {
    let now = narrow.read().clone();
    let genre = now.genre.clone().unwrap_or_default();
    let played = now.watched.map_or(ANY, watched_token);
    let stereo = match now.stereo {
        None => ANY,
        Some(true) => "3d",
        Some(false) => "2d",
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
                        narrow.with_mut(|n| n.watched = watched_of(&value));
                    },
                    option { value: ANY, selected: played == ANY, "All" }
                    option { value: "unwatched", selected: played == "unwatched", "Unwatched" }
                    option { value: "in-progress", selected: played == "in-progress", "In progress" }
                    option { value: "watched", selected: played == "watched", "Watched" }
                }
            }
            label { class: "pharos-filter",
                span { aria_hidden: "true", "3D" }
                select {
                    aria_label: "3D",
                    value: stereo,
                    onchange: move |e| {
                        let value = e.value();
                        narrow.with_mut(|n| {
                            n.stereo = match value.as_str() {
                                "3d" => Some(true),
                                "2d" => Some(false),
                                _ => None,
                            }
                        });
                    },
                    option { value: ANY, selected: stereo == ANY, "2D and 3D" }
                    option { value: "3d", selected: stereo == "3d", "3D only" }
                    option { value: "2d", selected: stereo == "2d", "2D only" }
                }
            }
            if let Some(mut grouped) = grouped {
                label { class: "pharos-filter",
                    span { aria_hidden: "true", "Collections" }
                    select {
                        aria_label: "Collections",
                        value: if *grouped.read() { "grouped" } else { "films" },
                        onchange: move |e| {
                            let on = e.value() == "grouped";
                            keep_collections(on);
                            grouped.set(on);
                        },
                        option { value: "grouped", selected: *grouped.read(), "Grouped" }
                        option { value: "films", selected: !*grouped.read(), "Each film" }
                    }
                }
            }
            if now != Narrow::default() {
                button {
                    class: "pharos-button pharos-button-secondary pharos-filter-reset",
                    r#type: "button",
                    onclick: move |_| narrow.set(Narrow::default()),
                    "Reset filters"
                }
            }
        }
    }
}

/// The device key of whether a Movies library groups its films into their
/// box sets (B1086).
pub const COLLECTIONS_KEY: &str = "pharos.library.collections";

/// Whether a Movies library groups its films into box sets, from what the
/// device keeps under [`COLLECTIONS_KEY`]: on unless turned off.
pub fn collections_on(kept: Option<&str>) -> bool {
    kept != Some("off")
}

/// [`collections_on`] for this device; a failed read keeps the default.
fn stored_collections() -> bool {
    match platform::stored_theme(COLLECTIONS_KEY) {
        Ok(kept) => collections_on(kept.as_deref()),
        Err(e) => {
            platform::warn(&format!("pharos: collections choice not read: {e}"));
            collections_on(None)
        }
    }
}

/// Keeps the device's choice to group films into box sets.
fn keep_collections(on: bool) {
    if let Err(e) = platform::store_theme(COLLECTIONS_KEY, if on { "on" } else { "off" }) {
        platform::warn(&format!("pharos: collections choice not kept: {e}"));
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
/// A library's grid also jumps by section, letters in name order (V252) and
/// years in a date order (B317/V256): the rail on the right goes straight to
/// one, and Shift+wheel steps to the next or previous, reading ahead through
/// the pages in between. Given a `place`, a `sessionStorage` key, it keeps
/// there where the viewer is and, to `restore`, opens there again (B560).
#[component]
fn PagedGrid(
    first: Page,
    source: Listing,
    rail: Option<Rail>,
    genres: Option<Signal<Vec<String>>>,
    place: Option<String>,
    #[props(default)] restore: bool,
    #[props(default)] square: Option<bool>,
    #[props(default)] chain: Option<Chain>,
) -> Element {
    let ctx = use_context::<Ctx>();
    let mut pager = use_signal(|| Pager::new(first.clone()));
    // The observer and the button fire outside any one render, so both send
    // here; `Pager::begin` is what keeps a second request out while one is
    // in flight.
    let paged = source.clone();
    let chained = chain.clone();
    let requests = use_coroutine(move |mut rx: UnboundedReceiver<Ask>| {
        let paged = paged.clone();
        let chained = chained.clone();
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
                // One listing, or the part of a chain this position is in.
                let (source, start, limit, chain) = match &chained {
                    Some(c) => {
                        let (part, start, limit) = c.step(start, limit);
                        (
                            c.listing(part).clone(),
                            start,
                            limit,
                            Some((c.clone(), part)),
                        )
                    }
                    None => (paged.clone(), start, limit, None),
                };
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
                        Ok(page) => match chain {
                            Some((c, part)) => {
                                let whole = c.whole(part, page.total, pager.peek().total());
                                pager.write().land_as(page, whole);
                            }
                            None => pager.write().land(page),
                        },
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
    // The grid keeps its loaded pages through a screen reload, so a mark
    // stored from one of its tiles is shown on that tile here (V1160).
    // The mark standing when the grid mounts is older than its pages, which
    // already show it or something newer, so only later ones apply.
    let stored = try_use_context::<StoredMark>();
    let mounted = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    use_effect(move || {
        let last = stored.and_then(|s| *s.0.read());
        if !mounted.replace(true) {
            return;
        }
        if let Some((id, played)) = last {
            if pager.peek().items().iter().any(|i| i.id == id) {
                pager.write().mark(id, played);
            }
        }
    });
    // An "In progress" grid offers each tile's watched mark (B1200/V1160).
    let mark = source.watched == Some(Watched::InProgress);
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

    // Where each section starts, and the genres for the filter bar.
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
    // Coming back to the grid is a jump to where the viewer was, its tile
    // set back as far below the top as it was (`land`).
    // A kept grid with nowhere to come back to opens at the top, not
    // wherever the page it replaced was scrolled to.
    let saved = use_hook(|| {
        let saved = place.as_deref().filter(|_| restore).and_then(read_place);
        if place.is_some() && saved.is_none() {
            platform::scroll_to_top();
        }
        saved
    });
    let arrive_from = saved.map_or(0, |(tile, _)| usize::try_from(tile).unwrap_or(usize::MAX));
    let mut jump = use_signal(|| saved.map(|(tile, _)| tile));
    let mut land = use_signal(|| saved.map(|(_, top)| top));
    let mut cursor = use_signal(|| None::<u32>);
    use_effect(move || {
        let Some(target) = jump() else {
            return;
        };
        // The rail takes room from the grid when it appears, which moves
        // the rows; a place to come back to waits for it.
        if rail.is_some() && land.peek().is_some() && index.read().is_none() {
            return;
        }
        let state = pager.read();
        let loaded = u32::try_from(state.items().len()).unwrap_or(u32::MAX);
        if target < loaded {
            let kept = land.take();
            let placed = match kept {
                Some(top) => platform::scroll_tile_to(target, top),
                None => platform::scroll_to_tile(target),
            };
            // The warp's pull was measured before the page scrolled the
            // arriving tiles into view.
            if placed && kept.is_some() {
                platform::stage_arrival(true);
            }
            if !placed {
                // A kept place that cannot be reached opens at the top (V470).
                if kept.is_some() {
                    platform::scroll_to_top();
                }
                platform::warn(&format!(
                    "pharos: grid tile {target} not in the page to scroll to"
                ));
            }
            jump.set(None);
        } else if state.failed() || !state.has_more() {
            if land.take().is_some() {
                platform::scroll_to_top();
            }
            jump.set(None);
        } else if !state.in_flight() {
            requests.send(Ask::Through(target));
        }
    });
    // Not while a jump is under way: its pages are still landing. `try_peek`:
    // this also runs as the grid is torn down, when the signal may be gone.
    let keep_place = move |key: &str| {
        if jump.try_peek().map_or(true, |j| j.is_some()) {
            return;
        }
        let Some(at) = platform::first_visible_tile() else {
            return;
        };
        let Some(top) = platform::tile_top(at) else {
            return;
        };
        if let Err(e) = platform::session_set(key, &format!("{at} {top}")) {
            platform::warn(&format!("pharos: grid place not kept: {e}"));
        }
    };
    // Kept once scrolling settles, or on a click inside the settle delay
    // (following a tile's link), never as the grid goes: by then the next
    // view has moved the page (V470).
    use_hook(move || {
        place.map(|key| std::rc::Rc::new(platform::on_scroll_settled(move || keep_place(&key))))
    });
    let mut go = move |target: u32| {
        land.set(None);
        cursor.set(Some(target));
        jump.set(Some(target));
    };
    let mut wheel = use_signal(Wheel::default);
    use_hook(move || {
        rail.is_some().then(|| {
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
    let shown: Vec<Item> = match &chain {
        Some(c) => c.shown(state.items()).into_iter().cloned().collect(),
        None => state.items().to_vec(),
    };
    // The sentinel is a place among the tiles shown, which a chain thins.
    let sentinel = state
        .has_more()
        .then(|| shown.len().saturating_sub(SENTINEL_LEAD));
    let page = Page {
        items: shown,
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
    let current = cursor()
        .and_then(|at| letters::letter_at(&starts, at))
        .map(str::to_owned);
    // Every letter, the ones the listing lacks greyed out; or the years the
    // listing holds, every few when there are many.
    let buttons: Vec<(String, Option<u32>)> = match rail {
        _ if starts.is_empty() => Vec::new(),
        None => Vec::new(),
        Some(Rail::Letters) => LETTERS
            .iter()
            .map(|l| {
                let label = l.to_string();
                let start = letters::start_of(&starts, &label);
                (label, start)
            })
            .collect(),
        Some(Rail::Years) => letters::shown(&starts)
            .into_iter()
            .map(|(label, start)| (label, Some(start)))
            .collect(),
    };
    // A thinned rail of years marks the shown year at or before the one in
    // view.
    let marked = current.as_deref().and_then(|c| {
        let at = letters::start_of(&starts, c)?;
        buttons
            .iter()
            .filter(|(_, s)| s.is_some_and(|s| s <= at))
            .max_by_key(|(_, s)| *s)
            .map(|(l, _)| l.clone())
    });
    let years = rail == Some(Rail::Years);
    rsx! {
        if !buttons.is_empty() {
            nav {
                class: if years { "pharos-letters pharos-years" } else { "pharos-letters" },
                aria_label: if years { "Jump to year" } else { "Jump to letter" },
                for (label, start) in buttons {
                    button {
                        key: "{label}",
                        class: "pharos-letter",
                        r#type: "button",
                        disabled: start.is_none(),
                        "aria-current": if marked.as_deref() == Some(label.as_str()) { "true" },
                        aria_label: if label == "#" {
                            if years { "No date" } else { "Numbers and symbols" }
                        },
                        onclick: move |_| {
                            if let Some(start) = start {
                                go(start);
                            }
                        },
                        "{label}"
                    }
                }
            }
        }
        PosterList { page, square, sentinel, arrive_from, mark }
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

/// How many tiles play the library transition (V476), from the first one
/// in view: more than a TV or desktop screen shows at once, so no tile in
/// view sits still while the ones before it arrive, and later pages land
/// without one.
const ARRIVING_TILES: usize = 60;

/// How many tiles before the one a grid reopens at also arrive (B579): a
/// place near the end of a library cannot be scrolled to the top of the
/// screen, so up to a screen of tiles above it stays in view.
const ARRIVING_LEAD: usize = 30;

/// The class and stagger of the tile at `at` in a grid that opens with the
/// tile at `from` at the top of the screen: the [`ARRIVING_LEAD`] tiles
/// before it and the [`ARRIVING_TILES`] from it carry `pharos-arrive`, and a
/// scattered place for the rain (`--fx-r`, 0 to 10); the rest carry none.
/// Each one's place in the wave comes from where it sits on screen
/// (web.rs `stage_arrival`, B578).
fn arrival(at: usize, from: usize, album: bool) -> (Option<&'static str>, Option<String>) {
    let album_class = album.then_some("pharos-album-tile");
    let first = from.saturating_sub(ARRIVING_LEAD);
    if at < first || at >= from + ARRIVING_TILES {
        return (album_class, None);
    }
    let class = if album {
        "pharos-album-tile pharos-arrive"
    } else {
        "pharos-arrive"
    };
    (Some(class), Some(format!("--fx-r: {}", (at * 7) % 11)))
}

/// The row and column on screen of each arriving tile, given each one's
/// top and height in page px in grid order and the page px at the top of
/// the viewport: the wave runs corner to corner by row plus column (B578)
/// and the flip turns row by row (V492), whatever the grid's width. Rows
/// count from the first that reaches into view, so a grid reopened part way
/// down starts its wave on screen (B579); rows above it are all row 0.
pub fn arrival_cells(tiles: &[(f64, f64)], view_top: f64) -> Vec<(usize, usize)> {
    // A tile starts a new row once its top is clearly below the row's.
    const SAME_ROW_PX: f64 = 1.0;
    let mut cells = Vec::with_capacity(tiles.len());
    let (mut row, mut col) = (0_usize, 0_usize);
    let mut row_top = tiles.first().map(|(top, _)| *top).unwrap_or_default();
    let mut first_in_view = None;
    for (at, (top, height)) in tiles.iter().enumerate() {
        if at > 0 && *top > row_top + SAME_ROW_PX {
            row += 1;
            col = 0;
            row_top = *top;
        } else if at > 0 {
            col += 1;
        }
        if first_in_view.is_none() && top + height > view_top {
            first_in_view = Some(row);
        }
        cells.push((row, col));
    }
    let skip = first_in_view.unwrap_or_default();
    cells
        .into_iter()
        .map(|(row, col)| (row.saturating_sub(skip), col))
        .collect()
}

/// How far the warp carries an arriving tile (V492): the offset in CSS px
/// from the tile's centre to the viewport's, which it flies out from, and
/// its distance from there as a share of the viewport's half diagonal (0 at
/// the middle, 1 at a corner, at most 2 below the fold), which orders the
/// ripple. `tile` is its left, top, width and height in viewport px.
pub fn arrival_pull(tile: (f64, f64, f64, f64), view: (f64, f64)) -> (f64, f64, f64) {
    const FARTHEST: f64 = 2.0;
    let (left, top, width, height) = tile;
    let (half_w, half_h) = (view.0 / 2.0, view.1 / 2.0);
    let dx = half_w - (left + width / 2.0);
    let dy = half_h - (top + height / 2.0);
    let corner = half_w.hypot(half_h);
    let share = if corner > 0.0 {
        (dx.hypot(dy) / corner).min(FARTHEST)
    } else {
        0.0
    };
    (dx, dy, share)
}

/// The tiles of a page; the one at `sentinel` carries [`SENTINEL_ID`].
#[component]
fn PosterList(
    page: Page,
    square: Option<bool>,
    sentinel: Option<usize>,
    #[props(default)] arrive_from: usize,
    /// Each tile gets a button that marks it watched, or unwatched again.
    #[props(default)]
    mark: bool,
) -> Element {
    // Each tile that arrives is placed in the wave by where it sits on
    // screen, once it is in the page.
    let count = page.items.len();
    use_effect(use_reactive!(|count| {
        let _ = count;
        platform::stage_arrival(false);
    }));
    let class = match square {
        Some(true) => "pharos-grid pharos-grid-square",
        _ => "pharos-grid",
    };
    let tiles = page.items.iter().enumerate().map(|(at, item)| {
        let arrive = arrival(at, arrive_from, item.kind == ItemKind::Album);
        (at, item, arrive)
    });
    rsx! {
        if page.items.is_empty() {
            p { class: "pharos-empty", if mark { "Nothing in progress here." } else { "Nothing in here yet." } }
        }
        ul { class,
            for (at , item , (tile_class , stagger)) in tiles {
                li {
                    key: "{item.id}",
                    id: if sentinel == Some(at) { SENTINEL_ID },
                    class: tile_class,
                    style: stagger,
                    "data-key-row": (item.kind == ItemKind::Album || mark).then_some("true"),
                    PosterTile { item: item.clone() }
                    if mark {
                        div { class: "pharos-tile-mark",
                            WatchedToggle {
                                id: item.id,
                                played: item.played,
                                what: (item.kind == ItemKind::Series).then(|| "show".to_owned()),
                                name: display_name(item),
                            }
                        }
                    }
                    if item.kind == ItemKind::Album {
                        QueueButtons {
                            name: item.name.clone(),
                            tracks: Vec::new(),
                            album: item.id,
                            kind: QueueTarget::AlbumCard,
                        }
                    }
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

/// A hero for a series, artist or album: art, name, facts and overview, and
/// for a series its watched mark.
#[component]
fn Hero(
    title: String,
    detail: Option<Detail>,
    facts: Vec<String>,
    square: bool,
    #[props(default)] mark: bool,
    /// Offers the favourite mark alone, as an album or artist does (B1085).
    #[props(default)]
    favourite: bool,
    /// For a series, the episode its Play or Resume starts.
    #[props(default)]
    next: Option<Item>,
) -> Element {
    let art_class = if square {
        "pharos-hero-art pharos-hero-art-square"
    } else {
        "pharos-hero-art"
    };
    let image = detail.as_ref().and_then(|d| d.item.image.clone());
    let album = detail
        .as_ref()
        .is_some_and(|d| d.item.kind == ItemKind::Album);
    let facts = facts.join(" · ");
    rsx! {
        header { class: "pharos-hero",
            div { class: art_class,
                match image {
                    Some(src) => rsx! { img { src: platform::art_src(&src), alt: "" } },
                    None if album => rsx! { ArtPlaceholder { kind: ItemKind::Album, name: title.clone() } },
                    None => rsx! {},
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
                if next.is_some() || ((mark || favourite) && detail.is_some()) {
                    div { class: "pharos-actions",
                        if let Some(ep) = next.as_ref() {
                            Link {
                                to: Route::Watch(ep.id),
                                class: "pharos-button",
                                id: MAIN_ACTION,
                                key_action: "play",
                                {next_label(ep)}
                            }
                        }
                        if let Some(d) = detail.as_ref().filter(|_| mark) {
                            WatchedToggle {
                                id: d.item.id,
                                played: d.item.played,
                                what: "show",
                                name: d.item.name.clone(),
                            }
                        }
                        if let Some(d) = detail.as_ref().filter(|_| mark || favourite) {
                            FavoriteToggle {
                                id: d.item.id,
                                favorite: d.item.favorite,
                                name: d.item.name.clone(),
                            }
                        }
                    }
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
pub fn ShowView(show: Show, #[props(default)] seen: Vec<MemberSeen>) -> Element {
    // A show opens at its top, not wherever the grid it came from was
    // scrolled to (B599); a season tab keeps the series, and the scroll.
    let series = show.series.as_ref().map(|d| d.item.id);
    use_effect(use_reactive!(|series| {
        let _ = series;
        platform::scroll_to_top();
    }));
    use_key_mode(Mode::Detail);
    use_arrival_focus(series, MAIN_ACTION);
    let in_group = in_this_group(&show, &seen);
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
    let marked_season = show.selected.filter(|_| !show.episodes.items.is_empty());
    rsx! {
        section { class: "pharos-show",
            Hero {
                title: show.title.clone(),
                detail: show.series.clone(),
                facts,
                square: false,
                mark: true,
                next: next_episode(&show.episodes.items).cloned(),
            }
            if let Some(series) = series {
                super::check_match::CheckMatchBadge { series }
            }
            if show.seasons.len() > 1 {
                nav { class: "pharos-tabs", aria_label: "Seasons", "data-key-tabs": "seasons",
                    for season in show.seasons.iter() {
                        Link {
                            key: "{season.id}",
                            to: Route::Browse(season.id),
                            current: Some(season.id) == show.selected,
                            class: if Some(season.id) == show.selected { "pharos-tab pharos-tab-active" } else { "pharos-tab" },
                            "{season.name}"
                        }
                    }
                }
            }
            if show.seasons.len() == 1 || marked_season.is_some() {
                div { class: "pharos-season-bar",
                    if show.seasons.len() == 1 {
                        h3 { class: "pharos-section-title", "{show.seasons[0].name}" }
                    }
                    if let Some(season) = marked_season {
                        WatchedToggle {
                            id: season,
                            played: all_played(&show.episodes, show.season_played),
                            what: "season",
                            name: show
                                .seasons
                                .iter()
                                .find(|s| s.id == season)
                                .map_or_else(String::new, |s| s.name.clone()),
                        }
                    }
                }
            }
            InThisGroup { rows: in_group }
            {remount_on(
                &show.selected.map_or_else(String::new, |s| s.to_string()),
                rsx! { EpisodeList { page: show.episodes.clone(), seen, source: episode_source(&show) } },
            )}
        }
    }
}

/// How to list the rest of a show page's episodes: the shown season's, or the
/// series' own when it has no seasons.
fn episode_source(show: &Show) -> Option<Listing> {
    let parent = show
        .selected
        .or_else(|| show.series.as_ref().map(|d| d.item.id))?;
    Some(Listing::of(parent, None, INDEX_ORDER))
}

/// The episode a show page's Play starts, from the season shown: the one
/// part-watched, else the first unwatched, else the first.
pub fn next_episode(episodes: &[Item]) -> Option<&Item> {
    episodes
        .iter()
        .find(|e| progress_percent(e).is_some())
        .or_else(|| episodes.iter().find(|e| !e.played))
        .or(episodes.first())
}

/// The show page's Play, naming the episode: "Resume S2 · E3".
fn next_label(episode: &Item) -> String {
    let verb = if progress_percent(episode).is_some() {
        "Resume"
    } else {
        "Play"
    };
    match episode_label(episode) {
        Some(label) => format!("{verb} {label}"),
        None => verb.to_owned(),
    }
}

/// The ids the show page asks the group about: the series, the season shown
/// and the season's episodes.
pub fn seen_ids(show: &Show) -> Vec<ItemRef> {
    show.series
        .iter()
        .map(|d| d.item.id)
        .chain(show.selected)
        .chain(show.episodes.items.iter().map(|e| e.id))
        .collect()
}

/// The marks the show page's "In this group" shows: the series' episode
/// counts, and the shown season's when there is one besides.
fn in_this_group(show: &Show, seen: &[MemberSeen]) -> Vec<(String, Vec<seen::SeenRow>)> {
    if seen.is_empty() {
        return Vec::new();
    }
    let series = show.series.as_ref().map(|d| d.item.id);
    match (series, show.selected) {
        (Some(series), Some(season)) if series != season => vec![
            ("Series".to_owned(), seen::rows(seen, series)),
            ("This season".to_owned(), seen::rows(seen, season)),
        ],
        (Some(id), _) | (None, Some(id)) => vec![(String::new(), seen::rows(seen, id))],
        (None, None) => Vec::new(),
    }
}

/// Whether `page` holds fewer episodes than the season has.
fn is_partial(page: &Page) -> bool {
    page.items.len() < page.total as usize
}

/// Whether every episode of a season is watched. A whole season on `page`
/// answers for itself; a longer one takes the server's `aggregate` for the
/// season, and reads as unwatched without one, which still offers to mark
/// it.
fn all_played(page: &Page, aggregate: Option<bool>) -> bool {
    if is_partial(page) {
        return aggregate.unwrap_or(false);
    }
    !page.items.is_empty() && page.items.iter().all(|e| e.played)
}

/// A season's episodes as rows: still, number and name, runtime, watched
/// state and progress. The row opens the episode; Play starts it. Given the
/// `source` listing, a season longer than one page offers "Show more" for the
/// rest (B821) and keeps what it loaded when a watched mark reloads the
/// screen.
#[component]
pub fn EpisodeList(
    page: Page,
    #[props(default)] seen: Vec<MemberSeen>,
    #[props(default)] source: Option<Listing>,
) -> Element {
    let ctx = try_use_context::<Ctx>();
    let mut pager = use_signal(|| Pager::new(page.clone()));
    // The fetch of a range of the listing, as `PagedGrid` makes it; `None`
    // when there is nothing to ask or no one to ask.
    let read = {
        let source = source.clone();
        move |start: u32, limit: u32| {
            let (ctx, source) = (ctx?, source.clone()?);
            let api = ctx.api.peek().clone()?;
            Some(async move {
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
                if let Err(e) = &result {
                    if e.needs_sign_in() {
                        let mut machine = ctx.machine;
                        machine.write().apply(Event::NeedsSignIn);
                    }
                }
                result
            })
        }
    };
    // A reloaded screen brings a fresh first page: swap it in, and read the
    // pages loaded after it again so their marks are fresh too.
    let reloaded = page.clone();
    let reread = read.clone();
    use_effect(use_reactive!(|reloaded| {
        let same = {
            let state = pager.peek();
            state.total() == reloaded.total && state.items().starts_with(&reloaded.items)
        };
        if same {
            return;
        }
        let Some((start, count)) = pager.write().refresh_head(reloaded) else {
            return;
        };
        match reread(start, count) {
            Some(fetch) => {
                spawn(async move {
                    match fetch.await {
                        Ok(tail) => pager.write().land_tail(start, tail),
                        Err(_) => pager.write().fail(),
                    }
                });
            }
            None => pager.write().fail(),
        }
    }));
    let mut more = move |trigger: Trigger| {
        let Some(start) = pager.write().begin(trigger) else {
            return;
        };
        match read(start, BROWSE_PAGE) {
            Some(fetch) => {
                spawn(async move {
                    match fetch.await {
                        Ok(next) => pager.write().land(next),
                        Err(_) => pager.write().fail(),
                    }
                });
            }
            None => pager.write().fail(),
        }
    };
    let state = pager.read();
    let paged = source.is_some() && state.has_more();
    let label = if state.in_flight() {
        "Loading…"
    } else if state.failed() {
        "Try again"
    } else {
        "Show more"
    };
    rsx! {
        if state.items().is_empty() {
            p { class: "pharos-empty", "No episodes yet." }
        }
        ol { class: "pharos-episodes", "data-keys-list": "episodes",
            for ep in state.items().iter() {
                li {
                    key: "{ep.id}",
                    "data-key-row": "episode",
                    EpisodeRow { episode: ep.clone(), seen: seen::rows(&seen, ep.id) }
                }
            }
        }
        if paged {
            p { class: "pharos-muted", "Showing {state.items().len()} of {state.total()}." }
            button {
                class: "pharos-button",
                r#type: "button",
                disabled: state.in_flight(),
                onclick: move |_| more(Trigger::Button),
                "{label}"
            }
        } else if source.is_none() {
            MoreNote { page: Page { items: state.items().to_vec(), total: state.total(), start: 0 } }
        }
    }
}

#[component]
fn EpisodeRow(episode: Item, #[props(default)] seen: Vec<seen::SeenRow>) -> Element {
    let progress = progress_percent(&episode);
    let label = match episode.episode {
        Some(n) if !is_placeholder_episode_name(&episode) => format!("{n}. {}", episode.name),
        _ => display_name(&episode),
    };
    let length = runtime(episode.runtime_ms);
    rsx! {
        div { class: "pharos-episode",
            Link { to: Route::Title(episode.id), class: "pharos-episode-link",
                div { class: "pharos-episode-still",
                    if let Some(src) = &episode.image {
                        Art { src: src.clone(), sizes: "11rem" }
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
                    if episode.damaged {
                        span { class: "pharos-episode-damaged", title: "The file is damaged", "Damaged" }
                    }
                    SeenDots { rows: seen }
                }
            }
            EpisodeMark { id: episode.id, label: label.clone(), played: episode.played }
            QueueButtons {
                name: label.clone(),
                tracks: vec![episode.clone()],
                album: None,
                kind: QueueTarget::EpisodeRow,
            }
            super::check_match::RematchEpisode { id: episode.id }
            Link {
                to: Route::Watch(episode.id),
                class: "pharos-button pharos-episode-play",
                key_action: "play",
                if progress.is_some() { "Resume" } else { "Play" }
            }
        }
    }
}

/// A round tick on an episode row that marks it watched, or unwatched once
/// it is.
#[component]
fn EpisodeMark(id: ItemRef, label: String, played: bool) -> Element {
    let mark = use_context::<MarkPlayed>();
    let what = if played {
        format!("Mark {label} unwatched")
    } else {
        format!("Mark {label} watched")
    };
    rsx! {
        button {
            r#type: "button",
            class: if played { "pharos-episode-mark is-watched" } else { "pharos-episode-mark" },
            title: "{what}",
            "aria-label": "{what}",
            "data-key-action": "watched",
            onclick: move |_| mark.0.call((id, !played, label.clone())),
            svg { view_box: "0 0 24 24", "aria-hidden": "true",
                path { d: "M5 12.5l4.5 4.5L19 7.5" }
            }
        }
    }
}

/// Which list a music library shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MusicTab {
    Artists,
    Albums,
    /// The mixes the listener can start (B901/V850).
    Playlists,
}

/// A music library: its artists, its albums or its playlists, switched by
/// tabs.
#[component]
pub fn MusicLibraryView(title: String, library: ItemRef, artists: Page, albums: Page) -> Element {
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
                button {
                    r#type: "button",
                    role: "tab",
                    class: tab_class(MusicTab::Playlists),
                    aria_selected: tab() == MusicTab::Playlists,
                    onclick: move |_| tab.set(MusicTab::Playlists),
                    "Playlists"
                }
            }
            // Each tab pages through its own listing (B821); switching tabs
            // mounts the other grid, which starts from the page loaded.
            match tab() {
                MusicTab::Artists => remount_on(
                    "artists",
                    rsx! {
                        PagedGrid {
                            first: artists.clone(),
                            source: Listing::of(library, Some(ItemKind::Artist), "SortName"),
                            rail: None,
                            square: Some(true),
                        }
                    },
                ),
                MusicTab::Playlists => rsx! { mixes::MixShelf {} },
                MusicTab::Albums => remount_on(
                    "albums",
                    rsx! {
                        PagedGrid {
                            first: albums.clone(),
                            source: Listing::of(library, Some(ItemKind::Album), "SortName"),
                            rail: None,
                            square: Some(true),
                        }
                    },
                ),
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
            Hero {
                title: detail.item.name.clone(),
                detail: Some(detail.clone()),
                facts,
                square: true,
                favourite: true,
            }
            {remount_on(
                &detail.item.id.to_string(),
                rsx! {
                    PagedGrid {
                        first: albums,
                        source: Listing::of(
                            detail.item.id,
                            Some(ItemKind::Album),
                            ARTIST_ALBUM_ORDER,
                        ),
                        rail: None,
                        square: Some(true),
                    }
                },
            )}
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
    let first_track = tracks.items.first().map(|t| t.id);
    // Opened from the keyboard, Enter plays the first track (spec 029).
    super::keys::use_arrival_focus(detail.item.id, FIRST_TRACK_ID);
    let discs = discs(&tracks.items);
    let multi = discs.len() > 1;
    let complete = tracks.items.len() >= tracks.total as usize;
    // Read once here, not per row, so a queue change redraws the list once.
    let queued: HashSet<ItemRef> = try_use_context::<QueueCtx>()
        .map(|q| q.0.read().upcoming().iter().map(|e| e.id).collect())
        .unwrap_or_default();
    rsx! {
        section { class: "pharos-album",
            Hero {
                title: detail.item.name.clone(),
                detail: Some(detail.clone()),
                facts,
                square: true,
                favourite: true,
            }
            if !tracks.items.is_empty() {
                QueueButtons {
                    name: detail.item.name.clone(),
                    // A long album lists past one page; fetch it whole.
                    tracks: if complete { tracks.items.clone() } else { Vec::new() },
                    album: detail.item.id,
                    kind: QueueTarget::AlbumPage,
                }
            }
            if tracks.items.is_empty() {
                p { class: "pharos-empty", "No tracks yet." }
            }
            for (disc, rows) in discs {
                div { key: "{disc}", class: "pharos-disc",
                    if multi {
                        h3 { class: "pharos-section-title", "Disc {disc}" }
                    }
                    ol { class: "pharos-tracks", "data-keys-list": "true",
                        for track in rows {
                            li { key: "{track.id}",
                                TrackRow {
                                    queued: queued.contains(&track.id),
                                    first: Some(track.id) == first_track,
                                    track,
                                }
                            }
                        }
                    }
                }
            }
            MoreNote { page: tracks }
        }
    }
}

/// The album page's first track, focused when the page opens.
const FIRST_TRACK_ID: &str = "pharos-first-track";

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
fn TrackRow(track: Item, queued: bool, #[props(default)] first: bool) -> Element {
    let length = track_length(track.runtime_ms);
    rsx! {
        div {
            class: "pharos-track-item",
            id: first.then_some(FIRST_TRACK_ID),
            "data-key-row": "true",
            TrackLink { track: track.clone(), length, queued }
            QueueButtons { name: track.name.clone(), tracks: vec![track], kind: QueueTarget::Song }
        }
    }
}

#[component]
fn TrackLink(track: Item, length: Option<String>, queued: bool) -> Element {
    rsx! {
        Link { to: Route::Watch(track.id), class: "pharos-track",
            span { class: "pharos-track-number",
                if let Some(n) = track.episode { "{n}" }
            }
            span { class: "pharos-track-name",
                "{track.name}"
                if queued {
                    span { class: "pharos-track-queued pharos-muted", "In queue" }
                }
            }
            span { class: "pharos-track-length pharos-muted",
                if let Some(l) = length { "{l}" }
            }
        }
    }
}

/// Where queued songs go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// "Play next": the front of the queue.
    Next,
    /// "Add to queue": the end.
    End,
}

impl Place {
    pub fn label(self) -> &'static str {
        match self {
            Place::Next => "next",
            Place::End => "end",
        }
    }
}

/// What a pair of queue buttons queues, and so how they look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueTarget {
    /// One song, on its row: icons only.
    Song,
    /// An album's songs, on its library card: icons over the cover.
    AlbumCard,
    /// An album's songs, on its own page: labelled buttons.
    AlbumPage,
    /// A film or an episode on its own page, for the video queue (V853):
    /// labelled buttons.
    VideoPage,
    /// An episode on its show's list, for the video queue: icons only.
    EpisodeRow,
}

impl QueueTarget {
    pub fn label(self) -> &'static str {
        match self {
            QueueTarget::Song => "song",
            QueueTarget::AlbumCard => "album_card",
            QueueTarget::AlbumPage => "album_page",
            QueueTarget::VideoPage => "video_page",
            QueueTarget::EpisodeRow => "episode_row",
        }
    }

    /// Whether it queues on the video queue rather than the music one.
    pub fn video(self) -> bool {
        matches!(self, QueueTarget::VideoPage | QueueTarget::EpisodeRow)
    }

    /// The note when nothing it named could be queued.
    fn nothing(self, name: &str) -> String {
        if self.video() {
            format!("{name} cannot be queued.")
        } else {
            format!("{name} has no songs to queue.")
        }
    }
}

/// How long the "Added" note stays.
const QUEUED_NOTE_MS: u32 = 2_500;

/// "Play next" and "Add to queue" (B436/V376). `tracks` are queued as
/// given; when there are none, `album`'s songs are fetched first. Without
/// the app's queue (a page rendered on its own) they draw nothing.
#[component]
pub fn QueueButtons(
    name: String,
    tracks: Vec<Item>,
    album: Option<ItemRef>,
    kind: QueueTarget,
) -> Element {
    let mut note = use_signal(|| None::<String>);
    // Counts notes, so a note's timer clears only its own note.
    let mut shown = use_signal(|| 0u64);
    let (next_label, end_label) = (
        format!("Play {name} next"),
        format!("Add {name} to the queue"),
    );
    let queue = if kind.video() {
        try_use_context::<VideoQueueCtx>().map(|VideoQueueCtx(q, _)| q)
    } else {
        try_use_context::<QueueCtx>().map(|q| q.0)
    };
    let ctx = try_use_context::<Ctx>();
    // A song row can also start a mix like it, playing at once (spec 024).
    // No mix in a SyncPlay group, where the group picks the songs.
    let in_group = try_use_context::<Together>()
        .as_ref()
        .is_some_and(|t| t.view.read().active);
    let mix = try_use_context::<MixCtx>().filter(|_| !in_group);
    let mix_song = (kind == QueueTarget::Song)
        .then(|| tracks.first().map(|t| t.id))
        .flatten()
        .filter(|_| mix.is_some());
    let mix_label = format!("Start a mix like {name}");
    // In a group the group picks what plays, and the player ignores the
    // video queue, so nothing offers to fill it (V853).
    let Some(mut queue) = queue.filter(|_| !(kind.video() && in_group)) else {
        return rsx! {};
    };
    // A row's buttons answer `a` and `A` only from that row: with the focus
    // elsewhere on a show's page, they would queue its first episode.
    let row_only = kind == QueueTarget::EpisodeRow;
    let put = move |place: Place| {
        let (tracks, name) = (tracks.clone(), name.clone());
        spawn(async move {
            let found = match (
                tracks.is_empty(),
                album,
                ctx.and_then(|c| c.api.peek().clone()),
            ) {
                (false, _, _) | (true, None, _) => Ok(tracks),
                (true, Some(album), Some(api)) => {
                    list_all(&api, &call("queue", "album"), album, None, INDEX_ORDER)
                        .await
                        .map_err(|e| e.to_string())
                }
                (true, Some(_), None) => Err("not signed in".to_owned()),
            };
            let target = kind.label();
            let place_label = place.label();
            match found {
                Ok(items) => {
                    let songs: Vec<Track> = if kind.video() {
                        items
                            .iter()
                            .filter(|i| video_kind(i.kind))
                            .map(Track::video)
                            .collect()
                    } else {
                        items
                            .iter()
                            .filter(|i| i.kind == ItemKind::Audio)
                            .map(Track::from)
                            .collect()
                    };
                    let count = songs.len();
                    platform::info(&added_line(place_label, target, count));
                    match place {
                        Place::Next => queue.write().play_next(songs),
                        Place::End => queue.write().add(songs),
                    }
                    note.set(Some(match (place, count) {
                        (_, 0) => kind.nothing(&name),
                        (Place::Next, _) => format!("{name} plays next."),
                        (Place::End, _) => format!("{name} added to the queue."),
                    }));
                }
                Err(e) => {
                    platform::warn(&format!(
                        "pharos: queue add failed place={place_label} target={target} album={album:?} error={e}"
                    ));
                    note.set(Some(format!("Could not queue {name}.")));
                }
            }
            let mine = *shown.peek() + 1;
            shown.set(mine);
            platform::sleep_ms(QUEUED_NOTE_MS).await;
            if *shown.peek() == mine {
                note.set(None);
            }
        });
    };
    let put_end = put.clone();
    let labelled = matches!(kind, QueueTarget::AlbumPage | QueueTarget::VideoPage);
    rsx! {
        div { class: "pharos-queue-buttons", "data-target": kind.label(),
            button {
                class: "pharos-queue-button",
                r#type: "button",
                title: "{next_label}",
                "data-key-action": "queue-next",
                "data-key-row-only": row_only.then_some("true"),
                aria_label: "{next_label}",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    put(Place::Next);
                },
                Icon { path: ICON_QUEUE_NEXT }
                if labelled { span { "Play next" } }
            }
            button {
                class: "pharos-queue-button",
                r#type: "button",
                title: "{end_label}",
                "data-key-action": "queue-end",
                "data-key-row-only": row_only.then_some("true"),
                aria_label: "{end_label}",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    put_end(Place::End);
                },
                Icon { path: ICON_QUEUE_ADD }
                if labelled { span { "Add to queue" } }
            }
            if let (Some(song), Some(MixCtx(mut mix))) = (mix_song, mix) {
                button {
                    class: "pharos-queue-button pharos-mix-from",
                    "data-key-action": "mix",
                    r#type: "button",
                    title: "{mix_label}",
                    aria_label: "{mix_label}",
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        mixes::start(&mut queue.write(), &mut mix.write(), "More like this".to_owned(), radio(song), true);
                    },
                    Icon { path: ICON_MIX }
                }
            }
            span { class: "pharos-queue-note", role: "status",
                if let Some(text) = note() { "{text}" }
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
    Ok(neighbours_and_rest(api, info, episode).await?.0)
}

/// [`neighbours`], and the items after `episode` in its season or on its
/// album, which seed the play queue (B435/V375).
pub async fn neighbours_and_rest<A: PharosApi>(
    api: &A,
    info: &CallInfo,
    episode: &Item,
) -> Result<(Neighbours, Vec<Item>), ClientError> {
    let Some(season) = episode.parent else {
        return Ok((Neighbours::default(), Vec::new()));
    };
    let episodes = |season| list_all(api, info, season, None, INDEX_ORDER);
    // The season's series is needed only at a season's edge, but asking for
    // it alongside the episodes costs no extra round trip.
    let (here, season_detail) = join(episodes(season), api.detail(info, season)).await;
    let here = here?;
    let Some(at) = here.iter().position(|e| e.id == episode.id) else {
        return Ok((Neighbours::default(), Vec::new()));
    };
    let mut near = Neighbours {
        previous: at.checked_sub(1).and_then(|i| here.get(i)).map(|e| e.id),
        next: here.get(at + 1).map(|e| e.id),
    };
    let rest = here.get(at + 1..).map(<[Item]>::to_vec).unwrap_or_default();
    if near.previous.is_some() && near.next.is_some() {
        return Ok((near, rest));
    }
    let Some(series) = season_detail?.item.parent else {
        return Ok((near, rest));
    };
    let mut seasons = list_all(api, info, series, Some(ItemKind::Season), "").await?;
    seasons.sort_by_key(|s| s.episode.unwrap_or(u32::MAX));
    let Some(s) = seasons.iter().position(|x| x.id == season) else {
        return Ok((near, rest));
    };
    let regular = |x: &&Item| x.episode != Some(0);
    if seasons.get(s).is_some_and(|x| !regular(&x)) {
        return Ok((near, rest));
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
    Ok((near, rest))
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

    use pharos_client::api::{
        Capabilities, Decision, Home, PlaybackGrant, StartTracks, TrackPicks,
    };

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
            artist: None,
            stereo: None,
            damaged: false,
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
            fonts: Vec::new(),
            artists: Vec::new(),
            album: None,
            cast: Vec::new(),
            versions: Vec::new(),
            damage: None,
            book: None,
            read_position: None,
        }
    }

    fn page(items: Vec<Item>) -> Page {
        Page {
            total: items.len() as u32,
            items,
            start: 0,
        }
    }

    #[test]
    fn the_tiles_from_the_first_in_view_arrive_and_each_has_its_place() {
        assert_eq!(
            arrival(0, 0, false),
            (Some("pharos-arrive"), Some("--fx-r: 0".to_owned()))
        );
        assert_eq!(
            arrival(3, 0, true),
            (
                Some("pharos-album-tile pharos-arrive"),
                Some("--fx-r: 10".to_owned())
            )
        );
        assert_eq!(
            arrival(ARRIVING_TILES - 1, 0, false).0,
            Some("pharos-arrive")
        );
        assert_eq!(arrival(ARRIVING_TILES, 0, false), (None, None));
        assert_eq!(
            arrival(ARRIVING_TILES, 0, true),
            (Some("pharos-album-tile"), None)
        );
        // B579: a grid that comes back scrolled to tile 150 animates the
        // tiles from a screen above there, not the ones far above it.
        let first = 150 - ARRIVING_LEAD;
        assert_eq!(arrival(first - 1, 150, false), (None, None));
        assert_eq!(
            arrival(first, 150, false).1,
            Some(format!("--fx-r: {}", (first * 7) % 11))
        );
        assert_eq!(
            arrival(150 + ARRIVING_TILES - 1, 150, false).0,
            Some("pharos-arrive")
        );
        assert_eq!(arrival(150 + ARRIVING_TILES, 150, false), (None, None));
        let scatter: Vec<usize> = (0..ARRIVING_TILES)
            .map(|at| {
                let style = arrival(at, 0, false).1.unwrap_or_default();
                style
                    .rsplit(' ')
                    .next()
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(99)
            })
            .collect();
        assert!(scatter.iter().all(|r| *r <= 10), "{scatter:?}");
    }

    #[test]
    fn arriving_tiles_know_their_row_and_column_on_screen() {
        // Three columns, rows 300 px apart, the last row short.
        let tiles: Vec<(f64, f64)> = [10.0, 10.0, 10.5, 310.0, 310.0, 310.0, 610.0]
            .iter()
            .map(|top| (*top, 280.0))
            .collect();
        assert_eq!(
            arrival_cells(&tiles, 0.0),
            vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (2, 0)]
        );
        // Scrolled so the first row is out of view: the second row leads.
        assert_eq!(
            arrival_cells(&tiles, 300.0),
            vec![(0, 0), (0, 1), (0, 2), (0, 0), (0, 1), (0, 2), (1, 0)]
        );
        assert!(arrival_cells(&[], 0.0).is_empty());
    }

    #[test]
    fn the_warp_flies_each_tile_out_from_the_middle_of_the_screen() {
        let view = (1000.0, 600.0);
        // A tile centred on the screen goes nowhere and leads the ripple.
        assert_eq!(
            arrival_pull((450.0, 250.0, 100.0, 100.0), view),
            (0.0, 0.0, 0.0)
        );
        // One in the top left corner starts from the middle, a whole half
        // diagonal away.
        let (dx, dy, share) = arrival_pull((-50.0, -50.0, 100.0, 100.0), view);
        assert_eq!((dx, dy), (500.0, 300.0));
        assert!((share - 1.0).abs() < 1e-9, "{share}");
        // Far below the fold it is last, but no later than twice that.
        assert_eq!(arrival_pull((450.0, 5000.0, 100.0, 100.0), view).2, 2.0);
        assert_eq!(arrival_pull((0.0, 0.0, 0.0, 0.0), (0.0, 0.0)).2, 0.0);
    }

    /// B490: switching libraries from the top bar keeps the same `GridView`
    /// mounted with new props, so the grid must start afresh for the new
    /// library rather than keep showing the one it opened on.
    #[test]
    fn a_switch_to_another_library_shows_that_library() {
        use futures_util::FutureExt as _;
        use std::rc::Rc;

        #[derive(Clone)]
        struct Props(Rc<RefCell<(u64, &'static str)>>);
        impl PartialEq for Props {
            fn eq(&self, _: &Self) -> bool {
                false
            }
        }
        #[component]
        fn Harness(shown: Props) -> Element {
            use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
            let api = use_signal(|| None::<pharos_client::api::NativeApi>);
            let machine = use_signal(super::super::machine::Machine::new);
            let marks = use_signal(|| 0);
            use_context_provider(|| Ctx {
                api,
                machine,
                marks,
            });
            let (id, name) = *shown.0.borrow();
            let kind = ItemKind::Movie;
            rsx! {
                GridView {
                    title: name.to_owned(),
                    page: page(vec![item(ItemRef::Real(id * 10), kind, name)]),
                    more: Some(Listing::of(ItemRef::Real(id), None, "SortName")),
                    library: true,
                }
            }
        }
        let settle = |dom: &mut VirtualDom| {
            for _ in 0..8 {
                if dom.wait_for_work().now_or_never().is_none() {
                    break;
                }
                dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
            }
        };
        let shown = Props(Rc::new(RefCell::new((1, "Films"))));
        let mut dom = VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                shown: shown.clone(),
            },
        );
        dom.rebuild_in_place();
        settle(&mut dom);
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(r#"href="/ui/title/10""#), "{html}");

        *shown.0.borrow_mut() = (2, "Shows");
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        settle(&mut dom);
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(r#"href="/ui/title/20""#), "{html}");
        assert!(!html.contains(r#"href="/ui/title/10""#), "{html}");
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
        /// Whether each listing asked for box sets folded (B1086).
        collapsed: RefCell<Vec<bool>>,
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
        async fn views(&self, _: &CallInfo) -> Result<Page, ClientError> {
            Ok(Page::default())
        }
        async fn query(
            &self,
            info: &CallInfo,
            listing: &Listing,
            start: u32,
            limit: u32,
        ) -> Result<Page, ClientError> {
            let (parent, group) = (listing.parent, listing.group);
            self.collapsed.borrow_mut().push(listing.collapse);
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
        async fn person(
            &self,
            _: &CallInfo,
            _: ItemRef,
        ) -> Result<pharos_client::api::PersonPage, ClientError> {
            Err(refused())
        }
        async fn lyrics(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: bool,
        ) -> Result<pharos_client::api::Lyrics, ClientError> {
            Ok(Default::default())
        }
        async fn mix(
            &self,
            _: &CallInfo,
            _: &pharos_client::mix::MixSpec,
            _: &[ItemRef],
            _: u32,
        ) -> Result<pharos_client::mix::MixBatch, ClientError> {
            Ok(Default::default())
        }
        async fn play(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: &str,
            _: &Capabilities,
            _: TrackPicks,
        ) -> Result<(Decision, NoGrant), ClientError> {
            Err(refused())
        }
        async fn start_tracks(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: &Capabilities,
        ) -> Result<StartTracks, ClientError> {
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
                sets: false,
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

    fn mixed_library(series: u64, stored: Vec<Item>) -> (Fake, ItemRef) {
        let lib = ItemRef::Synth(50);
        let mut fake = Fake::default();
        let mut l = item(lib, ItemKind::Library, "Mixed");
        l.library_kind = Some(LibraryKind::Mixed);
        fake.details.insert(lib, detail(l));
        fake.lists.insert(
            (lib, Some(ItemKind::Series)),
            (0..series)
                .map(|n| item(ItemRef::Synth(1000 + n), ItemKind::Series, &format!("S{n}")))
                .collect(),
        );
        fake.lists.insert((lib, None), stored);
        (fake, lib)
    }

    #[test]
    fn a_mixed_library_shows_series_in_place_of_their_episodes() {
        let stored = vec![
            item(ItemRef::Real(1), ItemKind::Episode, "Pilot"),
            item(ItemRef::Real(2), ItemKind::Movie, "Film"),
        ];
        let (fake, lib) = mixed_library(1, stored);
        let Browse::Mixed { page, chain, .. } =
            run(load(&fake, &CallInfo::default(), lib, |_| {})).unwrap()
        else {
            panic!("not a mixed library");
        };
        let names: Vec<_> = chain
            .shown(&page.items)
            .iter()
            .map(|i| i.name.clone())
            .collect();
        assert_eq!(names, ["S0", "Film"]);
        assert_eq!(chain.head_total, 1);
        assert_eq!(page.total, 3, "every place the server counts");
    }

    #[test]
    fn a_mixed_library_without_series_shows_what_it_stores() {
        let stored = vec![item(ItemRef::Real(1), ItemKind::Episode, "Pilot")];
        let (fake, lib) = mixed_library(0, stored);
        let Browse::Mixed { page, chain, .. } =
            run(load(&fake, &CallInfo::default(), lib, |_| {})).unwrap()
        else {
            panic!("not a mixed library");
        };
        assert_eq!(chain.shown(&page.items).len(), 1);
    }

    #[test]
    fn a_mixed_library_with_more_series_than_a_page_continues_them_first() {
        let stored = vec![item(ItemRef::Real(2), ItemKind::Movie, "Film")];
        let (fake, lib) = mixed_library(u64::from(BROWSE_PAGE) + 20, stored);
        let Browse::Mixed { page, chain, .. } =
            run(load(&fake, &CallInfo::default(), lib, |_| {})).unwrap()
        else {
            panic!("not a mixed library");
        };
        assert_eq!(page.items.len(), BROWSE_PAGE as usize, "series only so far");
        assert_eq!(page.total, BROWSE_PAGE + 21);
        let (part, start, limit) = chain.step(BROWSE_PAGE, BROWSE_PAGE);
        assert_eq!(chain.listing(part).group, Some(ItemKind::Series));
        assert_eq!((start, limit), (BROWSE_PAGE, 20));
    }

    #[test]
    fn a_music_library_names_itself_for_its_tabs_to_page() {
        let lib = ItemRef::Synth(60);
        let mut fake = Fake::default();
        let mut l = item(lib, ItemKind::Library, "Music");
        l.library_kind = Some(LibraryKind::Music);
        fake.details.insert(lib, detail(l));
        fake.lists.insert(
            (lib, Some(ItemKind::Artist)),
            (0..150)
                .map(|n| item(ItemRef::Synth(2000 + n), ItemKind::Artist, "A"))
                .collect(),
        );
        let Browse::Music {
            library, artists, ..
        } = run(load(&fake, &CallInfo::default(), lib, |_| {})).unwrap()
        else {
            panic!("not a music library");
        };
        assert_eq!(library, lib);
        assert_eq!((artists.items.len(), artists.total), (100, 150));
    }

    #[test]
    fn a_season_longer_than_a_page_offers_the_rest() {
        let episodes: Vec<Item> = (0..100)
            .map(|n| item(ItemRef::Real(n), ItemKind::Episode, "E"))
            .collect();
        let mut long = page(episodes);
        long.total = 150;
        let source = Some(Listing::of(S1, None, INDEX_ORDER));
        let (paged, plain) = (long.clone(), long);
        let html = render(move || {
            rsx! { EpisodeList { page: paged.clone(), source: source.clone() } }
        });
        assert!(html.contains("Showing 100 of 150."), "{html}");
        assert!(html.contains(">Show more<"), "{html}");
        let html = render(move || rsx! { EpisodeList { page: plain.clone() } });
        assert!(html.contains("Showing 100 of 150."), "{html}");
        assert!(!html.contains("Show more"), "{html}");
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

    /// B1085: an album or an artist can be made a favourite from its page,
    /// and shows when it is one; neither offers a watched mark.
    #[test]
    fn albums_and_artists_offer_a_favourite_mark() {
        let mut record = item(ItemRef::Synth(5), ItemKind::Album, "Record");
        record.favorite = true;
        let album = detail(record);
        let tracks = vec![item(ItemRef::Real(1), ItemKind::Audio, "Opener")];
        let html = render(move || {
            rsx! { AlbumView { detail: album.clone(), tracks: page(tracks.clone()) } }
        });
        assert!(
            html.contains(r#"data-key-action="favorite""#)
                && html.contains(r#"aria-pressed="true""#)
                && html.contains("pharos-favorite on"),
            "a favourite album says so: {html}"
        );
        assert!(!html.contains(r#"data-key-action="watched""#), "{html}");

        let artist = detail(item(ItemRef::Synth(6), ItemKind::Artist, "Band"));
        let albums = vec![item(ItemRef::Synth(5), ItemKind::Album, "Record")];
        let html = render(move || {
            rsx! { ArtistView { detail: artist.clone(), albums: page(albums.clone()) } }
        });
        assert!(
            html.contains(r#"data-key-action="favorite""#)
                && html.contains(r#"aria-pressed="false""#),
            "an artist can be made a favourite: {html}"
        );
    }

    /// A season route's tab names the season and the show; the series route
    /// names the show alone, whichever season it shows by default.
    #[test]
    fn a_season_page_is_named_with_its_season() {
        let show = Browse::Show(Show {
            title: "Show".into(),
            series: None,
            seasons: vec![
                item(S1, ItemKind::Season, "Season 1"),
                item(S2, ItemKind::Season, "Season 2"),
            ],
            selected: Some(S2),
            episodes: page(Vec::new()),
            season_played: None,
        });
        assert_eq!(show.name(S2), "Season 2 · Show");
        assert_eq!(show.name(SERIES), "Show");
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
            season_played: None,
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

    #[test]
    fn a_show_page_marks_each_episode_its_season_and_the_show_watched() {
        let mut pilot = item(ItemRef::Real(1), ItemKind::Episode, "Pilot");
        pilot.episode = Some(1);
        let mut second = item(ItemRef::Real(2), ItemKind::Episode, "Second");
        second.episode = Some(2);
        second.played = true;
        let mut series = item(ItemRef::Synth(9), ItemKind::Series, "Show");
        series.played = false;
        let show = |episodes: Vec<Item>| Show {
            title: "Show".into(),
            series: Some(detail(series.clone())),
            seasons: vec![item(S1, ItemKind::Season, "Season 1")],
            selected: Some(S1),
            episodes: page(episodes),
            season_played: None,
        };

        let part = show(vec![pilot.clone(), second.clone()]);
        let html = render(move || rsx! { ShowView { show: part.clone() } });
        assert!(html.contains("Mark 1. Pilot watched"), "{html}");
        assert!(
            html.contains("pharos-episode-mark is-watched")
                && html.contains("Mark 2. Second unwatched"),
            "{html}"
        );
        assert!(
            html.contains(">Season 1<") && html.contains(">Mark season watched<"),
            "{html}"
        );
        assert!(html.contains(">Mark show watched<"), "{html}");
        assert!(
            html.contains(r#"data-key-action="favorite""#)
                && html.contains(r#"aria-pressed="false""#),
            "the show can be made a favourite (B1082): {html}"
        );

        pilot.played = true;
        let all = show(vec![pilot, second]);
        let html = render(move || rsx! { ShowView { show: all.clone() } });
        assert!(html.contains(">Mark season unwatched<"), "{html}");
    }

    #[test]
    fn a_season_longer_than_its_page_offers_to_mark_it_watched() {
        let mut ep = item(ItemRef::Real(1), ItemKind::Episode, "Pilot");
        ep.played = true;
        let mut partial = page(vec![ep]);
        assert!(all_played(&partial, None));
        partial.total = 30;
        assert!(!all_played(&partial, None));
        assert!(!all_played(&page(vec![]), None));
        // A fully watched season longer than its page can still be
        // marked unwatched, from the server's season aggregate.
        assert!(all_played(&partial, Some(true)));
        assert!(!all_played(&partial, Some(false)));
        // A whole page answers for itself, whatever the aggregate says.
        partial.total = 1;
        assert!(all_played(&partial, Some(false)));
    }

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom = VirtualDom::new_with_props(
            |body: std::rc::Rc<dyn Fn() -> Element>| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                use_context_provider(|| {
                    MarkPlayed(EventHandler::new(|_: (ItemRef, bool, String)| {}))
                });
                use_context_provider(|| {
                    super::super::views::MarkFavorite(EventHandler::new(
                        |_: (ItemRef, bool, String)| {},
                    ))
                });
                body()
            },
            body,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// B1200 (V1160): an "In progress" grid gives each tile its watched
    /// mark, a show's naming the show; any other grid gives none.
    /// Disarm check: with `mark` left false, no button renders.
    #[test]
    fn an_in_progress_grid_offers_each_tile_its_watched_mark() {
        let mut film = item(ItemRef::Real(1), ItemKind::Movie, "Film A");
        film.position_ms = 60_000;
        film.runtime_ms = 600_000;
        let show = item(ItemRef::Synth(2), ItemKind::Series, "Show B");
        let tiles = page(vec![film, show]);
        let marked = tiles.clone();
        let html = render(
            move || rsx! { PosterList { page: marked.clone(), square: None, sentinel: None, mark: true } },
        );
        assert!(html.contains(">Mark watched<"), "{html}");
        assert!(html.contains(">Mark show watched<"), "{html}");
        let html = render(
            move || rsx! { PosterList { page: tiles.clone(), square: None, sentinel: None } },
        );
        assert!(!html.contains("Mark "), "{html}");
        let empty = render(
            || rsx! { PosterList { page: page(vec![]), square: None, sentinel: None, mark: true } },
        );
        assert!(empty.contains("Nothing in progress here."), "{empty}");
    }

    /// B801 (V751): an album with no cover heads its page with the music
    /// tile; a series with no poster keeps an empty frame.
    #[test]
    fn an_album_without_a_cover_heads_its_page_with_the_music_tile() {
        let album = detail(item(ItemRef::Synth(7), ItemKind::Album, "Mezzanine"));
        let html = render(move || {
            rsx! { Hero { title: "Mezzanine", detail: Some(album.clone()), facts: vec![], square: true } }
        });
        assert!(html.contains("pharos-art-song"), "{html}");

        let series = detail(item(ItemRef::Synth(8), ItemKind::Series, "Frieren"));
        let html = render(move || {
            rsx! { Hero { title: "Frieren", detail: Some(series.clone()), facts: vec![], square: false } }
        });
        assert!(!html.contains("pharos-art-song"), "{html}");
    }

    fn seen_by(id: &str, name: &str, items: Vec<seen::ItemSeen>) -> MemberSeen {
        MemberSeen {
            member: pharos_client::group::GroupMember {
                id: id.into(),
                name: name.into(),
                buffering: false,
                own_tracks: false,
            },
            items,
        }
    }

    fn show_of_two_episodes() -> Show {
        const S1: ItemRef = ItemRef::Synth(12);
        let mut one = item(ItemRef::Real(1), ItemKind::Episode, "Pilot");
        one.episode = Some(1);
        let mut two = item(ItemRef::Real(2), ItemKind::Episode, "Return");
        two.episode = Some(2);
        Show {
            title: "Show".into(),
            series: Some(detail(item(ItemRef::Synth(11), ItemKind::Series, "Show"))),
            seasons: vec![item(S1, ItemKind::Season, "Season 1")],
            selected: Some(S1),
            episodes: page(vec![one, two]),
            season_played: None,
        }
    }

    #[test]
    fn the_show_page_asks_about_the_series_the_season_and_its_episodes() {
        assert_eq!(
            seen_ids(&show_of_two_episodes()),
            vec![
                ItemRef::Synth(11),
                ItemRef::Synth(12),
                ItemRef::Real(1),
                ItemRef::Real(2)
            ]
        );
    }

    #[test]
    fn a_show_in_a_group_shows_counts_and_who_has_seen_each_episode() {
        let count = |id, seen, of| seen::ItemSeen {
            id,
            played: seen == of,
            position_pct: 0,
            played_count: seen,
            total_count: of,
        };
        let answer = vec![
            seen_by(
                "a",
                "Ada",
                vec![
                    count(ItemRef::Synth(11), 6, 10),
                    count(ItemRef::Synth(12), 1, 2),
                    seen::ItemSeen {
                        id: ItemRef::Real(1),
                        played: true,
                        position_pct: 0,
                        played_count: 0,
                        total_count: 0,
                    },
                ],
            ),
            seen_by("b", "Bo", vec![]),
        ];
        let show = show_of_two_episodes();
        let html = render(move || rsx! { ShowView { show: show.clone(), seen: answer.clone() } });
        assert!(html.contains("In this group"), "{html}");
        assert!(html.contains("6/10 episodes"), "{html}");
        assert!(html.contains("This season"), "{html}");
        assert!(html.contains("1/2 episodes"), "{html}");
        // The pilot row has a dot each; Ada's is seen, Bo's is not.
        assert!(html.contains("pharos-seen-dot is-seen"), "{html}");
        assert!(html.contains("pharos-seen-dot is-unseen"), "{html}");
    }

    #[test]
    fn a_show_outside_a_group_has_no_marks() {
        let show = show_of_two_episodes();
        let html = render(move || rsx! { ShowView { show: show.clone() } });
        assert!(!html.contains("In this group"), "{html}");
        assert!(!html.contains("pharos-seen-dot"), "{html}");
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
            watched: Some(Watched::Unwatched),
            ..Narrow::default().ordered(Order::Released)
        };
        let listing = released.apply(&base);
        assert_eq!(listing.sort_by, "PremiereDate,ProductionYear,SortName");
        assert_eq!(listing.genre.as_deref(), Some("Drama"));
        assert_eq!(listing.watched, Some(Watched::Unwatched));
        let three_d = Narrow {
            stereo: Some(true),
            ..Narrow::default()
        };
        assert_eq!(three_d.apply(&base).stereo, Some(true));
        let by_name = released.ordered(Order::Name);
        assert!(by_name.rail() == Rail::Letters && !by_name.descending);
        assert_eq!(by_name.apply(&base).sort_by, "SortName");
        assert_eq!(
            by_name.genre.as_deref(),
            Some("Drama"),
            "a new order keeps the filters"
        );
        assert_eq!(newest.rail(), Rail::Years);
    }

    #[test]
    fn a_library_choice_round_trips_through_its_query() {
        assert_eq!(
            Narrow::default().to_query(),
            "",
            "the default is the bare URL"
        );
        assert_eq!(Narrow::from_query(""), Narrow::default());
        for narrow in [
            Narrow::default().ordered(Order::Added),
            Narrow {
                descending: true,
                ..Narrow::default()
            },
            Narrow {
                descending: false,
                genre: Some("Sci-Fi & Fantasy".into()),
                watched: Some(Watched::Unwatched),
                ..Narrow::default().ordered(Order::Released)
            },
            Narrow {
                genre: Some("Comédie+ 100%".into()),
                watched: Some(Watched::Watched),
                ..Narrow::default()
            },
            Narrow {
                watched: Some(Watched::InProgress),
                ..Narrow::default()
            },
            Narrow {
                stereo: Some(false),
                ..Narrow::default()
            },
        ] {
            let query = narrow.to_query();
            assert!(
                !query.contains(' ') && query.matches('&').count() < 4,
                "{query}"
            );
            assert_eq!(Narrow::from_query(&query), narrow, "{query}");
        }
        assert_eq!(
            Narrow::default().ordered(Order::Added).to_query(),
            "?sort=added",
            "a date order's own direction is left out"
        );
    }

    #[test]
    fn a_library_query_reads_what_it_can_and_ignores_the_rest() {
        let narrow =
            Narrow::from_query("?sort=bogus&dir=up&genre=Sci-Fi+%26+Fantasy&watched=x&y=1");
        assert_eq!(
            narrow,
            Narrow {
                genre: Some("Sci-Fi & Fantasy".into()),
                ..Narrow::default()
            }
        );
        assert_eq!(
            Narrow::from_query("genre=100%2"),
            Narrow {
                genre: Some("100%2".into()),
                ..Narrow::default()
            },
            "a broken escape is kept as written"
        );
        assert_eq!(
            Narrow::from_query("genre=a%+1b").genre.as_deref(),
            Some("a% 1b"),
            "a sign is not a hex digit"
        );
    }

    #[test]
    fn order_tokens_round_trip() {
        for order in Order::ALL {
            assert_eq!(Order::from_token(order.token()), Some(order));
        }
        assert_eq!(Order::from_token("bogus"), None);
    }

    /// B810: collections and playlists open as the generic grid of their
    /// members, without the library filter bar.
    #[test]
    fn collections_and_playlists_load_as_a_plain_grid() {
        let mut fake = Fake::default();
        for (n, kind) in [(10, ItemKind::Collection), (11, ItemKind::Playlist)] {
            let id = ItemRef::Real(n);
            fake.details.insert(id, detail(item(id, kind, "Set")));
            assert_eq!(plan(fake.details.get(&id)), Plan::Grid, "{kind:?}");
            let opened = run(load(&fake, &CallInfo::default(), id, |_| {})).unwrap();
            assert!(
                matches!(opened, Browse::Grid { library: false, .. }),
                "{kind:?}"
            );
        }
    }

    /// B1086: a Movies library opens with its films grouped into box sets
    /// unless the device turned that off, and offers the choice; a Books
    /// library or a collection neither groups nor offers it.
    /// Disarm check: with `load` leaving `collapse` unset, the Movies
    /// listing asks for each film.
    #[test]
    fn a_movies_library_groups_its_box_sets_and_offers_the_choice() {
        let mut fake = Fake::default();
        for (n, kind) in [(9, LibraryKind::Movies), (8, LibraryKind::Books)] {
            let id = ItemRef::Real(n);
            let mut library = item(id, ItemKind::Library, "Lib");
            library.library_kind = Some(kind);
            fake.details.insert(id, detail(library));
        }
        let set = ItemRef::Real(10);
        fake.details
            .insert(set, detail(item(set, ItemKind::Collection, "Boxset")));
        let opened = |id| run(load(&fake, &CallInfo::default(), id, |_| {})).unwrap();
        let sets = |b: &Browse| matches!(b, Browse::Grid { sets: true, .. });
        assert!(sets(&opened(ItemRef::Real(9))));
        assert!(!sets(&opened(ItemRef::Real(8))));
        assert!(!sets(&opened(set)));
        assert_eq!(*fake.collapsed.borrow(), [true, false, false]);
        assert!(collections_on(None));
        assert!(collections_on(Some("on")));
        assert!(!collections_on(Some("off")));
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
    fn a_track_brings_the_rest_of_its_album_for_the_queue() {
        let mut fake = Fake::default();
        let album = ItemRef::Synth(31);
        fake.details
            .insert(album, detail(item(album, ItemKind::Album, "Album")));
        let track = |n| {
            let mut t = item(ItemRef::Real(n), ItemKind::Audio, "Track");
            t.parent = Some(album);
            t
        };
        fake.lists
            .insert((album, None), vec![track(40), track(41), track(42)]);
        let rest = |n| {
            run(neighbours_and_rest(&fake, &CallInfo::default(), &track(n)))
                .unwrap()
                .1
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(rest(40), [ItemRef::Real(41), ItemRef::Real(42)]);
        assert_eq!(rest(42), []);
    }

    fn with_queue(body: impl Fn() -> Element + 'static) -> String {
        render(move || {
            let queue = use_signal(crate::native::queue::Queue::default);
            use_context_provider(|| QueueCtx(queue));
            body()
        })
    }

    #[test]
    fn songs_and_albums_get_play_next_and_add_to_queue() {
        let mut song = item(ItemRef::Real(1), ItemKind::Audio, "Opener");
        song.episode = Some(1);
        let album = detail(item(ItemRef::Synth(5), ItemKind::Album, "Record"));
        let tracks = page(vec![song]);
        let html = with_queue({
            let (album, tracks) = (album.clone(), tracks.clone());
            move || rsx! { AlbumView { detail: album.clone(), tracks: tracks.clone() } }
        });
        for label in [
            "Play Opener next",
            "Add Opener to the queue",
            "Play Record next",
            "Add Record to the queue",
        ] {
            assert!(
                html.contains(&format!(r#"aria-label="{label}""#)),
                "{label}: {html}"
            );
        }
        // The album's page labels its buttons; a song's row has icons only.
        assert!(html.contains(r#"data-target="album_page""#), "{html}");
        assert!(html.contains(r#"data-target="song""#), "{html}");
        assert!(html.contains("<span>Play next</span>"), "{html}");
        // Buttons are not inside the row's link.
        let link = html
            .split_once(r#"class="pharos-track""#)
            .and_then(|(_, rest)| rest.split_once("</a>"))
            .map(|(inner, _)| inner)
            .expect("a track link");
        assert!(!link.contains("<button"), "{link}");

        let cards = page(vec![
            item(ItemRef::Synth(5), ItemKind::Album, "Record"),
            item(ItemRef::Synth(6), ItemKind::Artist, "Band"),
        ]);
        let grid = with_queue(move || rsx! { PosterGrid { page: cards.clone(), square: true } });
        assert!(grid.contains(r#"data-target="album_card""#), "{grid}");
        assert!(grid.contains(r#"aria-label="Play Record next""#), "{grid}");
        assert!(!grid.contains("Play Band next"), "{grid}");
    }

    #[test]
    fn without_the_apps_queue_there_are_no_queue_buttons() {
        let album = detail(item(ItemRef::Synth(5), ItemKind::Album, "Record"));
        let tracks = page(vec![item(ItemRef::Real(1), ItemKind::Audio, "Opener")]);
        let html =
            render(move || rsx! { AlbumView { detail: album.clone(), tracks: tracks.clone() } });
        assert!(!html.contains("pharos-queue-button"), "{html}");
    }

    #[test]
    fn a_queued_song_says_so_on_its_row() {
        let song = item(ItemRef::Real(1), ItemKind::Audio, "Opener");
        let album = detail(item(ItemRef::Synth(5), ItemKind::Album, "Record"));
        let tracks = page(vec![song.clone()]);
        let html = render(move || {
            let queue = use_signal(|| {
                let mut q = crate::native::queue::Queue::default();
                q.add(vec![Track::from(&song)]);
                q
            });
            use_context_provider(|| QueueCtx(queue));
            rsx! { AlbumView { detail: album.clone(), tracks: tracks.clone() } }
        });
        assert!(html.contains("In queue"), "{html}");
    }

    #[test]
    fn queue_labels_are_distinct() {
        assert_ne!(Place::Next.label(), Place::End.label());
        let targets = [
            QueueTarget::Song.label(),
            QueueTarget::AlbumCard.label(),
            QueueTarget::AlbumPage.label(),
        ];
        for (i, a) in targets.iter().enumerate() {
            assert!(!targets[i + 1..].contains(a), "{a}");
        }
    }

    #[test]
    fn a_film_has_no_neighbours() {
        let fake = Fake::default();
        let film = item(ItemRef::Real(3), ItemKind::Movie, "Film");
        let n = run(neighbours(&fake, &CallInfo::default(), &film)).unwrap();
        assert_eq!(n, Neighbours::default());
    }
}
