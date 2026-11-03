//! The browse screen's layouts (012 US2, US3, US5): what a library, series,
//! season, artist or album shows, chosen from its detail. A TV library is a
//! grid of series; a series or season is the show page, seasons as tabs over
//! an episode list; a music library offers its artists and albums; an artist
//! is a grid of albums; an album is its track list. Anything else, or a
//! parent whose detail cannot be read, is the plain grid of its children.

use dioxus::prelude::*;
use futures_util::future::{join3, try_join};
use futures_util::StreamExt as _;
use pharos_client::api::{
    ClientEvent, ClientEventKind, ClientLevel, Detail, Item, ItemKind, ItemRef, LibraryKind, Page,
    PharosApi,
};
use pharos_client::{CallInfo, ClientError};

use super::app::{call, Ctx};
use super::machine::Event;
use super::paging::{Pager, Trigger};
use super::platform;
use super::route::Route;
use super::views::{progress_percent, runtime, Link, PosterTile};

/// Most children one browse page lists.
pub const BROWSE_PAGE: u32 = 100;

/// `SortBy` for a season's episodes or an album's tracks: season (disc),
/// then episode (track) number.
const INDEX_ORDER: &str = "ParentIndexNumber,IndexNumber";

/// What a browse screen shows under a title that could not be fetched.
pub const BROWSE_TITLE_FALLBACK: &str = "Browse";

/// What `ui_action` a further page of a listing is fetched under.
pub const MORE_ACTION: &str = "more";

/// One paged listing: enough to ask for any page of it again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListSource {
    pub parent: ItemRef,
    pub group: Option<ItemKind>,
    pub sort: &'static str,
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
        more: Option<ListSource>,
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

/// Fetches the page of `source` that starts at `start`. Every failure is
/// logged through `warn` and reported through `report` as an `rpcFailed`
/// event, as a success is recorded (V237). `call` builds the `CallInfo`
/// from a route and action; this stamps it `browse` / `more`.
pub async fn fetch_more<A: PharosApi>(
    api: &A,
    call: impl Fn(&str, &str) -> CallInfo,
    source: &ListSource,
    start: u32,
    warn: impl Fn(String),
    report: impl Fn(ClientEvent),
) -> Result<Page, ClientError> {
    let info = call("browse", MORE_ACTION);
    let result = api
        .list(
            &info,
            source.parent,
            source.group,
            source.sort,
            start,
            BROWSE_PAGE,
        )
        .await;
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
    Ok(match (plan(detail.as_ref()), detail) {
        (Plan::SeriesGrid, _) => {
            let more = ListSource {
                parent: id,
                group: Some(ItemKind::Series),
                sort: "SortName",
            };
            Browse::Grid {
                title,
                page: list(more.parent, more.group, more.sort).await?,
                more: Some(more),
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
            let more = ListSource {
                parent: id,
                group: None,
                sort: INDEX_ORDER,
            };
            Browse::Grid {
                title,
                page: list(more.parent, more.group, more.sort).await?,
                more: Some(more),
            }
        }
        _ => {
            let more = ListSource {
                parent: id,
                group: None,
                sort: "SortName",
            };
            Browse::Grid {
                title,
                page: list(more.parent, more.group, more.sort).await?,
                more: Some(more),
            }
        }
    })
}

#[component]
pub fn BrowseLayout(browse: Browse) -> Element {
    match browse {
        Browse::Grid { title, page, more } => rsx! { GridView { title, page, more } },
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
pub fn GridView(title: String, page: Page, more: Option<ListSource>) -> Element {
    rsx! {
        section { class: "pharos-browse",
            h2 { "{title}" }
            match more {
                Some(source) => rsx! {
                    PagedGrid { key: "{source.parent}", first: page, source }
                },
                None => rsx! { PosterGrid { page } },
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

/// A grid that loads the rest of its listing as the viewer nears the end
/// (V237): an observer on a tile `SENTINEL_LEAD` before the last, and on
/// the end of the list, asks for the next page, and a button does the same by
/// hand and retries a failure.
#[component]
fn PagedGrid(first: Page, source: ListSource) -> Element {
    let ctx = use_context::<Ctx>();
    let mut pager = use_signal(|| Pager::new(first.clone()));
    // The observer and the button fire outside any one render, so both send
    // here; `Pager::begin` is what keeps a second request out while one is
    // in flight.
    let requests = use_coroutine(move |mut rx: UnboundedReceiver<Trigger>| async move {
        while let Some(trigger) = rx.next().await {
            let Some(start) = pager.write().begin(trigger) else {
                continue;
            };
            let Some(api) = ctx.api.peek().clone() else {
                pager.write().fail();
                continue;
            };
            spawn(async move {
                let result = fetch_more(
                    &api,
                    call,
                    &source,
                    start,
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
    });
    let mut watch = use_signal(|| None::<platform::SentinelWatch>);
    // Runs once the tiles are in the page, and again after every change to
    // the pager so the observer is re-armed on the tile that is now
    // `SENTINEL_LEAD` from the end (it reports a tile already in view).
    use_effect(move || {
        let armed = pager.read().sentinel().is_some();
        watch.set(if armed {
            platform::watch_sentinel(&[SENTINEL_ID, END_ID], move || {
                requests.send(Trigger::Scroll)
            })
        } else {
            None
        });
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
    rsx! {
        PosterList { page, sentinel: state.sentinel() }
        if state.has_more() {
            div { id: END_ID, aria_hidden: "true" }
            p { class: "pharos-muted", "Showing {state.items().len()} of {state.total()}." }
            button {
                class: "pharos-button",
                r#type: "button",
                disabled: state.in_flight(),
                onclick: move |_| requests.send(Trigger::Button),
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
    let facts = facts.join(" · ");
    rsx! {
        header { class: "pharos-hero",
            div { class: art_class,
                if let Some(src) = image {
                    img { src: "{src}", alt: "" }
                }
            }
            div { class: "pharos-hero-body",
                h2 { "{title}" }
                if !facts.is_empty() {
                    p { class: "pharos-muted", "{facts}" }
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
        async fn list(
            &self,
            info: &CallInfo,
            parent: ItemRef,
            group: Option<ItemKind>,
            sort_by: &str,
            start: u32,
            limit: u32,
        ) -> Result<Page, ClientError> {
            self.asked
                .borrow_mut()
                .push((parent, group, sort_by.into()));
            self.stamped.borrow_mut().push((
                info.ui_route.clone(),
                info.ui_action.clone(),
                start,
                limit,
            ));
            if self.refuse_lists {
                return Err(refused());
            }
            Ok(page(
                self.lists
                    .get(&(parent, group))
                    .cloned()
                    .unwrap_or_default(),
            ))
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
                more: Some(ListSource {
                    parent: id,
                    group: None,
                    sort: "SortName",
                }),
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

    fn bulk_source() -> ListSource {
        ListSource {
            parent: BULK,
            group: None,
            sort: "SortName",
        }
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
}
