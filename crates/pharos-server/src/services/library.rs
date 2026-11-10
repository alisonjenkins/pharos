//! The library: what a user may browse (spec 017 US4). Both wire layers list
//! libraries through here, so the Jellyfin views and `Library.views` cannot
//! drift apart.

use pharos_core::{
    DomainError, ItemPerson, LibraryKind, MediaItem, MediaKind, MediaStore as _, PersonKind,
    ReadPositionStore as _, User, UserDataStore as _, UserItemData, UserPolicy,
};

use crate::services::access::ItemHidden;
use crate::state::AppState;

/// The id of the one placeholder view served when no library and no media
/// root is configured, so a client's sidebar still renders.
pub const ALL_MEDIA_ID: &str = "00000000000000000000000000000000";

/// One library as a user sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryView {
    /// The library's stable wire id (32 hex).
    pub id: String,
    pub name: String,
    /// `Mixed` for an untyped root and for the placeholder.
    pub kind: LibraryKind,
    /// The representative item whose Primary image stands for the library,
    /// when it has one.
    pub primary_tag: Option<String>,
}

/// The libraries `policy` lets its user see (V184). A hidden library is
/// dropped before its image lookup, so it costs nothing.
///
/// Typed libraries reconciled from config come first; without any, one
/// untyped view per configured media root; without those, the
/// [`ALL_MEDIA_ID`] placeholder.
pub async fn views(state: &AppState, policy: &UserPolicy) -> Vec<LibraryView> {
    // Cloned out of the guard: the loop awaits, and a std RwLock guard held
    // across an await can deadlock the executor.
    let libs: Vec<pharos_core::Library> = state.libraries().iter().cloned().collect();
    if !libs.is_empty() {
        // Sequential: the first image lookup warms the whole synth map, so
        // the rest are memo hits.
        let mut out = Vec::with_capacity(libs.len());
        for lib in libs
            .into_iter()
            .filter(|l| policy.allows_library(&l.wire_id))
        {
            let primary_tag = crate::api::jellyfin::images::synth_primary_tag(state, &lib.wire_id)
                .await
                .and_then(|mut tags| tags.remove("Primary"));
            out.push(LibraryView {
                id: lib.wire_id,
                name: lib.name,
                kind: lib.kind,
                primary_tag,
            });
        }
        return out;
    }
    if state.media_roots.is_empty() {
        return if policy.allows_library(ALL_MEDIA_ID) {
            vec![LibraryView {
                id: ALL_MEDIA_ID.to_owned(),
                name: "All Media".to_owned(),
                kind: LibraryKind::Mixed,
                primary_tag: None,
            }]
        } else {
            Vec::new()
        };
    }
    state
        .media_roots
        .iter()
        .filter_map(|root| {
            let id = crate::api::jellyfin::items::library_id_for_root(root);
            policy.allows_library(&id).then(|| LibraryView {
                id,
                name: root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Media")
                    .to_owned(),
                kind: LibraryKind::Mixed,
                primary_tag: None,
            })
        })
        .collect()
}

/// Which stored kinds a listing keeps. The Jellyfin layer parses it from
/// `IncludeItemTypes=` (`items::parse_kind_filter`); `Library.latest` names
/// no kinds and passes `Unset`.
///
/// B153 — the distinction that was missing. `MusicVideo` names a type pharos
/// stores none of, and the old code read "zero recognised kinds" as "no kind
/// filter", so a request for music videos returned the ENTIRE library: 51.6 MB
/// of JSON for a query whose honest answer is an empty list. "I asked for X"
/// must never widen to "I asked for anything".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum KindFilter {
    /// No kind named: every kind is acceptable.
    #[default]
    Unset,
    /// At least one named type is a kind pharos stores.
    Kinds(Vec<MediaKind>),
    /// Types were named and none of them can appear in this result.
    MatchesNothing,
}

impl KindFilter {
    /// Whether this filter names `kind`. `Unset` names none, so a listing
    /// that must opt in to a kind (books in `resume`) does not for it.
    pub fn names(&self, kind: MediaKind) -> bool {
        matches!(self, KindFilter::Kinds(wanted) if wanted.contains(&kind))
    }

    /// `items` with only the kinds this filter keeps.
    pub fn keep(&self, items: Vec<MediaItem>) -> Vec<MediaItem> {
        match self {
            KindFilter::Unset => items,
            KindFilter::MatchesNothing => Vec::new(),
            KindFilter::Kinds(wanted) => items
                .into_iter()
                .filter(|i| wanted.contains(&i.kind))
                .collect(),
        }
    }
}

/// The most a "latest" listing returns, whatever the caller asks for.
pub const LATEST_MAX: u32 = 100;

/// Why an item listing could not be built.
#[derive(Debug)]
pub enum ListError {
    /// The user's library allow-list could not be applied.
    Hidden(ItemHidden),
    /// The item list or the user's play state could not be read.
    Store(DomainError),
}

/// The items `user` may see under `parent_id` (a library, series, season or
/// other wire id; `None` for all), of the kinds `kinds` keeps, newest first,
/// each with the user's play state. At most `limit`, capped at
/// [`LATEST_MAX`]; `None` means the cap.
pub async fn latest(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    kinds: &KindFilter,
    limit: Option<u32>,
) -> Result<Vec<(MediaItem, UserItemData)>, ListError> {
    let mut typed = newest_items(state, user, parent_id, kinds, limit, false).await?;
    crate::services::metadata_text::localize_items(state, user.id, typed.iter_mut()).await;
    items_with_user_data(state, user, typed).await
}

/// `items`, each with `user`'s play state, in one read.
async fn items_with_user_data(
    state: &AppState,
    user: &User,
    items: Vec<MediaItem>,
) -> Result<Vec<(MediaItem, UserItemData)>, ListError> {
    let ids: Vec<u64> = items.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(ListError::Store)?;
    Ok(items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            (item, ud)
        })
        .collect())
}

/// [`latest`]'s rows, without play state or localised text; `shallow`
/// leaves out the per-stream and long-text fields (`query_shallow`).
async fn newest_items(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    kinds: &KindFilter,
    limit: Option<u32>,
    shallow: bool,
) -> Result<Vec<MediaItem>, ListError> {
    use crate::api::jellyfin::items::{
        apply_policy_scope, resolve_parent_filter, ParentResolution,
    };
    use pharos_core::{MediaQuery, SortDir, SortKey};
    let limit = limit.unwrap_or(LATEST_MAX).min(LATEST_MAX);
    if limit == 0 {
        return Ok(Vec::new());
    }
    // B97 — "Latest" means most-recently-ADDED first (DateCreated desc), so a
    // newly scanned film surfaces in the home "Recently Added" row; id breaks
    // ties so the order is stable. Home-page rows must match the library the
    // user clicked into, and jellyfin-web's "Latest Movies" row filters to
    // Type=Movie.
    let mut mq = MediaQuery {
        limit: Some(limit),
        sort: vec![
            (SortKey::DateCreated, SortDir::Desc),
            (SortKey::Id, SortDir::Desc),
        ],
        ..MediaQuery::default()
    };
    match kinds {
        KindFilter::Unset => {}
        KindFilter::MatchesNothing => return Ok(Vec::new()),
        KindFilter::Kinds(wanted) => mq.kinds.clone_from(wanted),
    }
    match resolve_parent_filter(state, parent_id)
        .await
        .map_err(ListError::Store)?
    {
        ParentResolution::Empty => return Ok(Vec::new()),
        ParentResolution::All => {}
        ParentResolution::Filter(pf) => mq.parent = Some(pf),
        ParentResolution::PathPrefix(p) => mq.filters.path_prefix = Some(p),
        ParentResolution::GenreProbe(t) => mq.filters.genre_probe_token = Some(t),
    }
    apply_policy_scope(&mut mq, &user.policy, &state.parental_ratings);
    let (typed, _total) = if shallow {
        state.stores.query_shallow(&mq).await
    } else {
        state.stores.query(&mq).await
    }
    .map_err(ListError::Store)?;
    Ok(typed)
}

/// One tile of a latest rail (spec 038 FR-106).
#[derive(Debug)]
pub enum LatestTile {
    /// An item added on its own.
    Item(Box<MediaItem>, UserItemData),
    /// The series or album several of the newest items belong to: its DTO,
    /// dated by its newest child, and how many of them there are.
    Group {
        dto: serde_json::Value,
        new_children: u32,
    },
}

/// What a newly added item folds under on a latest rail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Fold {
    Own(u64),
    /// An episode, by its series' identity key (spec 033).
    Series(String),
    /// A track, by its album name, as the album listing keys it.
    Album(String),
}

impl Fold {
    fn of(item: &MediaItem) -> Self {
        match (item.kind, item.series.as_ref(), item.probe.album.as_deref()) {
            (MediaKind::Episode, Some(series), _) => Fold::Series(series.series_key().to_string()),
            (MediaKind::Audio, _, Some(album)) if !album.is_empty() => {
                Fold::Album(album.to_owned())
            }
            _ => Fold::Own(item.id),
        }
    }
}

/// The kind of tile a latest rail shows. The `label()` strings are a
/// dashboard contract for `pharos_latest_tiles_total{tile}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    /// An item listed on its own.
    Item,
    /// A series folding several new episodes.
    Series,
    /// An album folding several new tracks.
    Album,
}

impl TileKind {
    pub const ALL: [TileKind; 3] = [TileKind::Item, TileKind::Series, TileKind::Album];

    pub const fn label(self) -> &'static str {
        match self {
            TileKind::Item => "item",
            TileKind::Series => "series",
            TileKind::Album => "album",
        }
    }
}

/// A latest rail under `parent_id`: the newest [`LATEST_MAX`] items, with the
/// episodes of one series and the tracks of one album folded into one tile
/// placed where its newest child was; a series or album with a single new
/// child shows that child. At most `limit` tiles (`None` is the cap). Films
/// and books are unchanged. The fold reads shallow rows; the items listed on
/// their own are read in full.
pub async fn latest_tiles(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    kinds: &KindFilter,
    limit: Option<u32>,
) -> Result<Vec<LatestTile>, ListError> {
    use std::collections::HashMap;
    let limit = limit.unwrap_or(LATEST_MAX).min(LATEST_MAX) as usize;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let newest = newest_items(state, user, parent_id, kinds, Some(LATEST_MAX), true).await?;
    let read = newest.len();
    let mut groups: Vec<(Fold, Vec<MediaItem>)> = Vec::new();
    let mut at: HashMap<Fold, usize> = HashMap::new();
    for item in newest {
        let fold = Fold::of(&item);
        if let Some(group) = at.get(&fold).and_then(|&i| groups.get_mut(i)) {
            group.1.push(item);
        } else if groups.len() < limit {
            at.insert(fold.clone(), groups.len());
            groups.push((fold, vec![item]));
        }
    }
    let folds: Vec<Vec<&MediaItem>> = groups
        .iter()
        .filter(|(_, g)| g.len() > 1)
        .map(|(_, g)| g.iter().collect())
        .collect();
    let mut dtos = crate::api::jellyfin::items::latest_group_dtos(state, user.id, &folds)
        .await
        .inspect_err(|error| {
            tracing::warn!(parent = parent_id.unwrap_or("all"), %error, "latest rail: series or album tiles unavailable");
        })
        .map_err(ListError::Store)?
        .into_iter();
    // Each slot: a folded tile, or the id of the item listed on its own.
    let mut slots: Vec<Result<(LatestTile, TileKind), u64>> = Vec::with_capacity(groups.len());
    for (fold, group) in &groups {
        let newest_id = group.first().map_or(0, |i| i.id);
        if group.len() == 1 {
            slots.push(Err(newest_id));
            continue;
        }
        let kind = match fold {
            Fold::Album(_) => TileKind::Album,
            _ => TileKind::Series,
        };
        match dtos.next().flatten() {
            Some(dto) => slots.push(Ok((
                LatestTile::Group {
                    dto,
                    new_children: u32::try_from(group.len()).unwrap_or(u32::MAX),
                },
                kind,
            ))),
            None => {
                tracing::warn!(
                    parent = parent_id.unwrap_or("all"),
                    ?fold,
                    children = group.len(),
                    "latest rail: no tile for a fold, listing its newest child"
                );
                slots.push(Err(newest_id));
            }
        }
    }
    let single_ids: Vec<u64> = slots
        .iter()
        .filter_map(|s| s.as_ref().err().copied())
        .collect();
    let mut singles = state
        .stores
        .get_many(&single_ids)
        .await
        .map_err(ListError::Store)?;
    crate::services::metadata_text::localize_items(state, user.id, singles.iter_mut()).await;
    let mut singles: HashMap<u64, (MediaItem, UserItemData)> =
        items_with_user_data(state, user, singles)
            .await?
            .into_iter()
            .map(|row| (row.0.id, row))
            .collect();
    let tiles: Vec<(LatestTile, TileKind)> = slots
        .into_iter()
        .filter_map(|slot| match slot {
            Ok(tile) => Some(tile),
            // A row deleted since the fold read it is skipped.
            Err(id) => singles
                .remove(&id)
                .map(|(item, data)| (LatestTile::Item(Box::new(item), data), TileKind::Item)),
        })
        .collect();
    for (_, kind) in &tiles {
        metrics::counter!("pharos_latest_tiles_total", "tile" => kind.label()).increment(1);
    }
    tracing::debug!(
        parent = parent_id.unwrap_or("all"),
        read,
        tiles = tiles.len(),
        folded = tiles.iter().filter(|(_, k)| *k != TileKind::Item).count(),
        "latest rail folded"
    );
    Ok(tiles.into_iter().map(|(tile, _)| tile).collect())
}

/// One page of a listing, with the number of matching items before paging.
#[derive(Debug)]
pub struct ListPage {
    pub items: Vec<(MediaItem, UserItemData)>,
    pub total: u32,
    pub start: u32,
}

/// The most part-read books `resume` considers (FR-012).
const PART_READ_MAX: u32 = 200;

/// The items `user` has started and not finished, of the kinds `kinds`
/// keeps, in the store's resume order, each with the user's play state.
/// `start` and `limit` page the list; `total` counts every match (B164).
/// When `kinds` names books it also lists those with a saved read position
/// that are not played (FR-012), newest activity first among the rest.
pub async fn resume(
    state: &AppState,
    user: &User,
    kinds: &KindFilter,
    start: u32,
    limit: Option<u32>,
) -> Result<ListPage, ListError> {
    let mut ids = state
        .stores
        .resumable_items(user.id)
        .await
        .map_err(ListError::Store)?;
    // When each part-read book was last read, to order it among the rest.
    let mut read_at: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
    if kinds.names(MediaKind::Book) {
        let books = state
            .stores
            .part_read_items(user.id, PART_READ_MAX)
            .await
            .map_err(ListError::Store)?;
        let positions = state
            .stores
            .read_positions_bulk(user.id, &books)
            .await
            .map_err(ListError::Store)?;
        for (id, position) in books.iter().zip(positions) {
            if let Some(position) = position {
                read_at.insert(*id, position.updated_at);
                if !ids.contains(id) {
                    ids.push(*id);
                }
            }
        }
    }
    crate::services::access::retain_visible(state, user, &mut ids, |id| *id, "resume")
        .await
        .map_err(ListError::Hidden)?;
    // One query for every resumed id; a row deleted since it was resumed is skipped.
    let items = state
        .stores
        .get_many(&ids)
        .await
        .map_err(ListError::Store)?;
    let mut items = kinds.keep(items);
    if !read_at.is_empty() {
        let all: Vec<u64> = items.iter().map(|i| i.id).collect();
        let played_at = state
            .stores
            .user_data_bulk(user.id, &all)
            .await
            .map_err(ListError::Store)?;
        let when = |id: u64, last_played_at: i64| {
            read_at.get(&id).copied().unwrap_or(0).max(last_played_at)
        };
        let mut stamped: Vec<(i64, MediaItem)> = items
            .into_iter()
            .zip(played_at)
            .map(|(item, data)| (when(item.id, data.last_played_at), item))
            .collect();
        // Stable, so equal stamps keep the store's resume order.
        stamped.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        items = stamped.into_iter().map(|(_, item)| item).collect();
    }
    let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
    let start = start.min(total);
    let end = limit.map_or(total, |n| start.saturating_add(n)).min(total);
    let mut page: Vec<MediaItem> = items
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect();
    crate::services::metadata_text::localize_items(state, user.id, page.iter_mut()).await;
    let ids: Vec<u64> = page.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(ListError::Store)?;
    Ok(ListPage {
        items: page
            .into_iter()
            .enumerate()
            .map(|(i, item)| (item, user_data.get(i).copied().unwrap_or_default()))
            .collect(),
        total,
        start,
    })
}

/// How many series a "next up" listing covers when the caller names no limit.
pub const NEXT_UP_DEFAULT: u32 = 100;

/// What one Next Up listing came to. The `label()` strings are a dashboard
/// contract for `pharos_next_up_total{outcome}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextUpOutcome {
    /// At least one episode listed.
    Listed,
    /// Nothing to list.
    Empty,
    /// The listing could not be built.
    Error,
}

impl NextUpOutcome {
    pub const ALL: [NextUpOutcome; 3] = [
        NextUpOutcome::Listed,
        NextUpOutcome::Empty,
        NextUpOutcome::Error,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            NextUpOutcome::Listed => "listed",
            NextUpOutcome::Empty => "empty",
            NextUpOutcome::Error => "error",
        }
    }

    fn record(self) {
        metrics::counter!("pharos_next_up_total", "outcome" => self.label()).increment(1);
    }
}

/// For each series `user` has started and may see (or only `series_id`, a
/// series wire id), the episode to watch next under ADR-0113, the series
/// watched most recently first, each with the user's play state. At most
/// `limit` (at least 1); `None` means [`NEXT_UP_DEFAULT`].
pub async fn next_up(
    state: &AppState,
    user: &User,
    series_id: Option<&str>,
    limit: Option<u32>,
) -> Result<ListPage, ListError> {
    let listed = next_up_page(state, user, series_id, limit).await;
    match &listed {
        Ok((page, considered)) => {
            let outcome = if page.items.is_empty() {
                NextUpOutcome::Empty
            } else {
                NextUpOutcome::Listed
            };
            outcome.record();
            tracing::debug!(
                user_id = %user.id.0.simple(),
                scoped = series_id.is_some(),
                considered,
                kept = page.items.len(),
                outcome = outcome.label(),
                "next up listed"
            );
        }
        Err(error) => {
            NextUpOutcome::Error.record();
            tracing::warn!(user_id = %user.id.0.simple(), ?error, "next up failed");
        }
    }
    listed.map(|(page, _)| page)
}

/// [`next_up`]'s page, with the number of series it chose from.
async fn next_up_page(
    state: &AppState,
    user: &User,
    series_id: Option<&str>,
    limit: Option<u32>,
) -> Result<(ListPage, usize), ListError> {
    use crate::api::jellyfin::dto::series_id_for_key;
    use std::collections::HashMap;
    // B92 — the kotlin SDK sends `?SeriesId=` dashed; canonicalise once.
    let series_id_want =
        series_id.map(|s| crate::api::jellyfin::items::canonical_wire_id(s).into_owned());
    // FR-105 — the viewer's own rows say which series are started.
    let started = state
        .stores
        .started_items(user.id, MediaKind::Episode)
        .await
        .map_err(ListError::Store)?;
    let empty = ListPage {
        items: Vec::new(),
        total: 0,
        start: 0,
    };
    if started.is_empty() && series_id_want.is_none() {
        return Ok((empty, 0));
    }
    let rows: HashMap<u64, UserItemData> = started.into_iter().collect();
    let all = state.list_items_cached().await.map_err(ListError::Store)?;
    let all = crate::services::access::visible_items(state, user, all, "next_up")
        .await
        .map_err(ListError::Hidden)?;
    // A Series-page request scopes to that series (`SeriesId`); the
    // home-screen row leaves it unset and gets one pick per started series.
    let in_scope = |item: &MediaItem| -> bool {
        let Some(series) = item.series.as_ref() else {
            return false;
        };
        if !matches!(item.kind, MediaKind::Episode) {
            return false;
        }
        match series_id_want.as_deref() {
            None => true,
            Some(want) => {
                series_id_for_key(series.identity_folder(), &series.series_name).as_str() == want
            }
        }
    };
    // LIB-C11 — bucket by the folder-keyed identity, so two same-name
    // shows in distinct folders stay separate.
    let mut episodes: HashMap<String, (bool, Vec<&MediaItem>)> = HashMap::new();
    for item in all.iter().filter(|i| in_scope(i)) {
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        let bucket = episodes.entry(series.series_key().to_string()).or_default();
        bucket.0 |= rows.contains_key(&item.id);
        bucket.1.push(item);
    }
    let scoped = series_id_want.is_some();
    let episodes: Vec<Vec<&MediaItem>> = episodes
        .into_values()
        .filter_map(|(started, eps)| (started || scoped).then_some(eps))
        .collect();
    let considered = episodes.len();
    let data = |id: u64| rows.get(&id).copied().unwrap_or_default();
    let mut picks: Vec<(i64, &MediaItem)> = episodes
        .into_iter()
        .filter_map(|mut eps| {
            eps.sort_by_key(|e| (airing_order(e), e.id));
            pick_next(&eps, &data, scoped)
        })
        .collect();
    // FR-103 — the series watched most recently first; name breaks ties.
    picks.sort_by(|(a_at, a), (b_at, b)| {
        b_at.cmp(a_at)
            .then_with(|| series_name(a).cmp(series_name(b)))
    });
    picks.truncate(limit.unwrap_or(NEXT_UP_DEFAULT).max(1) as usize);
    if picks.is_empty() {
        return Ok((empty, considered));
    }
    let total = u32::try_from(picks.len()).unwrap_or(u32::MAX);
    // A pick is unplayed, so its row (favourite, chosen tracks) is rarely
    // among the started ones: read it for the picks alone.
    let pick_ids: Vec<u64> = picks.iter().map(|(_, i)| i.id).collect();
    let pick_data = state
        .stores
        .user_data_bulk(user.id, &pick_ids)
        .await
        .map_err(ListError::Store)?;
    let mut items: Vec<(MediaItem, UserItemData)> = picks
        .into_iter()
        .zip(pick_data)
        .map(|((_, item), data)| (item.clone(), data))
        .collect();
    crate::services::metadata_text::localize_items(
        state,
        user.id,
        items.iter_mut().map(|(i, _)| i),
    )
    .await;
    Ok((
        ListPage {
            items,
            total,
            start: 0,
        },
        considered,
    ))
}

/// An episode's place in airing order: season, then episode number.
fn airing_order(e: &MediaItem) -> (u32, u32) {
    let series = e.series.as_ref();
    (
        series.and_then(|s| s.season_number).unwrap_or(0),
        series.and_then(|s| s.episode_number).unwrap_or(0),
    )
}

fn series_name(e: &MediaItem) -> &str {
    e.series.as_ref().map_or("", |s| s.series_name.as_str())
}

fn is_special(e: &MediaItem) -> bool {
    e.series.as_ref().and_then(|s| s.season_number) == Some(0)
}

/// The Next Up episode of one series, `eps` in airing order, with when the
/// series was last played (ADR-0113). The anchor is the episode played or
/// started most recently, the later in airing order on a tie. The pick is
/// the first unplayed episode after it, a special only when the anchor is a
/// special (FR-102). On the home rail a series with nothing started is not
/// listed, and a pick already in progress is left to Continue Watching
/// (FR-101, FR-104); a `scoped` (Series page) request lists the anchor
/// itself when it is in progress, and an unstarted series' first episode.
fn pick_next<'a>(
    eps: &[&'a MediaItem],
    data: &impl Fn(u64) -> UserItemData,
    scoped: bool,
) -> Option<(i64, &'a MediaItem)> {
    let touched = |d: &UserItemData| d.played || d.last_played_position_ticks > 0;
    let anchor = eps
        .iter()
        .enumerate()
        .filter(|(_, e)| touched(&data(e.id)))
        .max_by_key(|(idx, e)| (data(e.id).last_played_at, *idx));
    let Some((at_idx, anchor)) = anchor else {
        if !scoped {
            return None;
        }
        let first = eps
            .iter()
            .find(|e| !is_special(e) && !data(e.id).played)
            .copied()?;
        return Some((0, first));
    };
    let anchor_data = data(anchor.id);
    let last_played_at = anchor_data.last_played_at;
    if !anchor_data.played {
        return scoped.then_some((last_played_at, *anchor));
    }
    let specials = is_special(anchor);
    let next = eps
        .iter()
        .skip(at_idx.saturating_add(1))
        .find(|e| (specials || !is_special(e)) && !data(e.id).played)
        .copied()?;
    if !scoped && data(next.id).last_played_position_ticks > 0 {
        return None;
    }
    Some((last_played_at, next))
}

/// Most people a title page lists from its cast.
pub const CAST_LIMIT: usize = 40;

/// Who appears in stored item `id`: its actors in billed order, then its
/// guest stars. Crew are left out.
pub async fn cast(state: &AppState, id: u64) -> Result<Vec<ItemPerson>, DomainError> {
    use pharos_core::PersonStore as _;
    let (mut cast, guests): (Vec<_>, Vec<_>) = state
        .stores
        .people_for_item(id)
        .await?
        .into_iter()
        .filter(|p| matches!(p.kind, PersonKind::Actor | PersonKind::GuestStar))
        .partition(|p| p.kind == PersonKind::Actor);
    cast.extend(guests);
    cast.truncate(CAST_LIMIT);
    Ok(cast)
}

/// The regular cast of the synthesised series or season `wire_id`, ranked
/// across its episodes' actor credits (guest stars left out). Empty for any
/// other synthesised item.
pub async fn ensemble(state: &AppState, wire_id: &str) -> Result<Vec<ItemPerson>, DomainError> {
    use pharos_core::PersonStore as _;
    let items = state.list_items_cached().await?;
    let episodes: Vec<u64> = restrict_to_parent(state, &items, Some(wire_id))
        .await
        .into_iter()
        .filter(|i| i.kind == MediaKind::Episode && i.series.is_some())
        .map(|i| i.id)
        .collect();
    let credits = state.stores.actor_credits_for_items(&episodes).await?;
    tracing::debug!(
        wire_id,
        episodes = episodes.len(),
        credits = credits.len(),
        "ensemble cast"
    );
    Ok(pharos_core::ensemble_cast(credits, CAST_LIMIT))
}

/// Drop items that don't live under the configured root / series /
/// season mapped to `parent_id`. Unknown `parent_id` → empty list.
/// The All-Media placeholder + `None` pass everything through.
pub async fn restrict_to_parent(
    state: &AppState,
    items: &[MediaItem],
    parent_id: Option<&str>,
) -> Vec<MediaItem> {
    use crate::api::jellyfin::dto::{
        album_id_for, artist_id_for, genre_id_for, season_id_for_key, series_id_for_key,
    };
    use pharos_core::{
        CollectionStore, GenreStore, LibraryStore, PersonStore, StudioStore, TagStore,
    };
    let Some(pid) = parent_id else {
        return items.to_vec();
    };
    // B82 — accept the dashed UUID the kotlin SDK re-emits, not just the
    // dashless form jellyfin-web echoes.
    let canonical = crate::api::jellyfin::items::canonical_wire_id(pid);
    let pid: &str = canonical.as_ref();
    if pid.is_empty() || pid == "00000000000000000000000000000000" {
        return items.to_vec();
    }
    // 1) Library / root match. LIB-C1 — prefer the typed `libraries` table:
    //    resolve the ParentId by wire_id to the set of item ids assigned
    //    that library (backfilled by path-prefix at boot, kept current by
    //    the scanner). Fall back to the legacy path-prefix scan over
    //    `media_roots` for libraries not yet backfilled (or test states
    //    wired only with `with_media_roots`).
    let matched_lib = state
        .libraries()
        .iter()
        .find(|l| l.wire_id == pid)
        .map(|l| (l.wire_id.clone(), l.root_path.clone()));
    if let Some((wire_id, root_path)) = matched_lib {
        if let Ok(ids) = state.stores.item_ids_for_library(&wire_id).await {
            if !ids.is_empty() {
                let want: std::collections::HashSet<u64> = ids.into_iter().collect();
                return items
                    .iter()
                    .filter(|i| want.contains(&i.id))
                    .cloned()
                    .collect();
            }
        }
        // No backfilled rows yet → fall back to the path-prefix scan
        // against this library's root so the view still resolves.
        let root = std::path::PathBuf::from(&root_path);
        return items
            .iter()
            .filter(|i| i.path.starts_with(&root))
            .cloned()
            .collect();
    }
    if let Some(root) = state
        .media_roots
        .iter()
        .find(|r| crate::api::jellyfin::items::library_id_for_root(r) == pid)
    {
        return items
            .iter()
            .filter(|i| i.path.starts_with(root))
            .cloned()
            .collect();
    }
    // 2) Series id → every episode whose folder-keyed series id matches
    //    pid. LIB-C11: resolves via the show FOLDER (falling back to the
    //    bare name for legacy rows), so a same-name show in another folder
    //    isn't pulled in under the wrong series.
    if items.iter().any(|i| {
        i.series
            .as_ref()
            .is_some_and(|s| series_id_for_key(s.identity_folder(), &s.series_name) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series
                    .as_ref()
                    .is_some_and(|s| series_id_for_key(s.identity_folder(), &s.series_name) == pid)
            })
            .cloned()
            .collect();
    }
    // 3) Season id → every episode in that (folder, season) pair.
    if items.iter().any(|i| {
        i.series.as_ref().is_some_and(|s| {
            s.season_number
                .is_some_and(|n| season_id_for_key(s.identity_folder(), &s.series_name, n) == pid)
        })
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    s.season_number.is_some_and(|n| {
                        season_id_for_key(s.identity_folder(), &s.series_name, n) == pid
                    })
                })
            })
            .cloned()
            .collect();
    }
    // 4) Artist id → every track whose artist or album_artist matches.
    if items.iter().any(|i| {
        i.probe
            .artist
            .as_deref()
            .is_some_and(|a| artist_id_for(a) == pid)
            || i.probe
                .album_artist
                .as_deref()
                .is_some_and(|a| artist_id_for(a) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.probe
                    .artist
                    .as_deref()
                    .is_some_and(|a| artist_id_for(a) == pid)
                    || i.probe
                        .album_artist
                        .as_deref()
                        .is_some_and(|a| artist_id_for(a) == pid)
            })
            .cloned()
            .collect();
    }
    // 5) Album id → every track whose album hashes to pid.
    if items.iter().any(|i| {
        i.probe
            .album
            .as_deref()
            .is_some_and(|a| album_id_for(a) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.probe
                    .album
                    .as_deref()
                    .is_some_and(|a| album_id_for(a) == pid)
            })
            .cloned()
            .collect();
    }
    // 6) Genre id → every item tagged with that genre. LIB-C4: resolve
    //    via the genres.wire_id → item_genres → items indexed join when
    //    the genre is a real row (the entity-backed exact pivot). If the
    //    wire id matches no genre row yet (rows scanned before C4 and not
    //    backfilled), fall back to the legacy in-memory probe.genre scan.
    if let Ok(ids) = state.stores.item_ids_for_genre(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    if items.iter().any(|i| {
        i.probe
            .genre
            .as_deref()
            .map(pharos_core::split_genre_field)
            .is_some_and(|gs| gs.iter().any(|g| genre_id_for(g) == pid))
    }) {
        return items
            .iter()
            .filter(|i| {
                i.probe
                    .genre
                    .as_deref()
                    .map(pharos_core::split_genre_field)
                    .is_some_and(|gs| gs.iter().any(|g| genre_id_for(g) == pid))
            })
            .cloned()
            .collect();
    }
    // 7) Person id → every item crediting that person. LIB-C2: resolve
    //    via people.wire_id → item_people → items indexed join. People
    //    have no legacy probe column, so there is no in-memory fallback
    //    (an unknown wire id simply yields no items → empty library).
    if let Ok(ids) = state.stores.item_ids_for_person(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // 8) Studio id → every item tagged with that studio. LIB-C3: resolve
    //    via studios.wire_id → item_studios → items indexed join. Studios
    //    have no legacy probe column (the old /Studios stub borrowed
    //    album_artist but never linked it), so there is no in-memory
    //    fallback — an unknown wire id yields no items → empty library.
    if let Ok(ids) = state.stores.item_ids_for_studio(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // 9) Collection / box set id → its members in curated sort_order.
    //    LIB-C5: resolve via collections.wire_id → collection_items →
    //    items, returning the members in the join's `sort_order` (NOT the
    //    id order of `items`) so a curated box set renders in order. The
    //    membership ids are the source of truth for both membership AND
    //    order; we index `items` by id and emit in member order. A wire id
    //    matching a *known but empty* collection still resolves (to an
    //    empty list) so a freshly-created empty box set browses cleanly,
    //    rather than falling through to the unknown-id empty Vec.
    if let Ok(collection) = state.stores.collection_by_wire_id(pid).await {
        if collection.is_some() {
            let member_ids = state.stores.collection_items(pid).await.unwrap_or_default();
            let mut by_id: std::collections::HashMap<pharos_core::MediaId, MediaItem> =
                items.iter().map(|i| (i.id, i.clone())).collect();
            return member_ids
                .iter()
                .filter_map(|id| by_id.remove(id))
                .collect();
        }
    }
    // 10) Tag id → every item carrying that tag. LIB-C6: resolve via
    //     tags.wire_id → item_tags → items indexed join. Tags have no
    //     legacy probe column, so there is no in-memory fallback — an
    //     unknown wire id yields no items → empty library.
    if let Ok(ids) = state.stores.item_ids_for_tag(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // Unknown id — render an empty library.
    Vec::new()
}

/// What a stored-item listing asks for. The vocabulary is Jellyfin's
/// (`SortBy` / `SortOrder` / `Filters` tokens, comma lists), so `/Items` and
/// the native `Library.query` share one parser and one query builder
/// (`items::build_media_query`). `limit: None` returns every match.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    pub start_index: u32,
    pub limit: Option<u32>,
    pub search_term: Option<String>,
    pub kinds: KindFilter,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub filters: Option<String>,
    pub ids: Option<String>,
    pub sort_seed: Option<u64>,
    pub genres: Option<String>,
    /// One genre by its name, matched through the genre entity rather than
    /// the raw probe string `genres` compares, so an item tagged "Action,
    /// Sci-Fi" is in both. A series matches on its show metadata's genres.
    pub genre: Option<String>,
    pub tags: Option<String>,
    pub name_starts_with: Option<String>,
    pub name_starts_with_or_greater: Option<String>,
    pub name_less_than: Option<String>,
    pub exclude_item_types: Option<String>,
    pub media_types: Option<String>,
    pub is_favorite: Option<bool>,
    pub is_played: Option<bool>,
    pub min_index_number: Option<u32>,
    pub max_index_number: Option<u32>,
    pub has_subtitles: Option<bool>,
    pub audio_languages: Option<String>,
    pub subtitle_languages: Option<String>,
    pub is_4k: Option<bool>,
    pub is_hd: Option<bool>,
    pub is_3d: Option<bool>,
    pub min_width: Option<u32>,
    pub max_width: Option<u32>,
    /// Fold the films of a box set into one tile for the set (B1086): see
    /// [`query_folded`].
    pub collapse_collections: bool,
}

/// How many items a native `Library.query` page holds when the caller names
/// no limit, and the most it holds when one is named: a page is one message,
/// and 1000 summaries keep it well under capnp's default traversal limit.
pub const QUERY_DEFAULT: u32 = 100;
pub const QUERY_MAX: u32 = 1000;

/// One page of stored items, with the number of matches before paging.
#[derive(Debug)]
pub struct QueryPage {
    pub items: Vec<MediaItem>,
    pub total: u32,
    pub start: u32,
}

/// The stored items `user` may see under `parent_id` that match `q`, one
/// page of them. The library allow-list and parental rating narrow the SQL
/// query itself, so `total` and the page offsets count only what the user
/// may see. An unknown `parent_id` matches nothing.
///
/// `SortBy=Random` cannot be a stable SQL order: the filtered set is fetched
/// unpaged, shuffled with a seed (the query's, else one from the user id, so
/// paging does not reshuffle), then tag- and play-state-filtered and paged.
///
/// Descriptions are in the user's language (ADR-0062).
pub async fn query(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    q: &ItemQuery,
) -> Result<QueryPage, DomainError> {
    let mut page = query_unlocalized(state, user, parent_id, q).await?;
    crate::services::metadata_text::localize_items(state, user.id, page.items.iter_mut()).await;
    Ok(page)
}

/// One page of a stored listing with the films of each box set folded into
/// one tile for the set, standing where its first film stood (B1086). A set
/// folds only when the listing holds two or more of its films, so a lone
/// member stays a film. The listing is read unpaged, then folded and paged,
/// so `total` and the offsets count tiles.
pub async fn query_folded(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    q: &ItemQuery,
) -> Result<FoldedPage, DomainError> {
    let folded = fold_page(state, user, parent_id, q).await;
    let outcome = if folded.is_ok() { "ok" } else { "error" };
    metrics::counter!("pharos_collection_fold_total", "outcome" => outcome).increment(1);
    folded
}

async fn fold_page(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    q: &ItemQuery,
) -> Result<FoldedPage, DomainError> {
    let unpaged = ItemQuery {
        start_index: 0,
        limit: None,
        ..q.clone()
    };
    let listed = query_unlocalized(state, user, parent_id, &unpaged).await?;
    let folded = fold_into_collections(state, listed.items).await?;
    let total = u32::try_from(folded.len()).unwrap_or(u32::MAX);
    let take = q.limit.map_or(usize::MAX, |l| l as usize);
    let mut entries: Vec<Folded> = folded
        .into_iter()
        .skip(q.start_index as usize)
        .take(take)
        .collect();
    let ids: Vec<u64> = entries
        .iter()
        .filter_map(|e| match e {
            Folded::Item(item, _) => Some(item.id),
            Folded::Collection { .. } => None,
        })
        .collect();
    let user_data = state.stores.user_data_bulk(user.id, &ids).await?;
    let mut user_data = user_data.into_iter();
    for entry in &mut entries {
        if let Folded::Item(_, data) = entry {
            *data = user_data.next().unwrap_or_default();
        }
    }
    Ok(FoldedPage {
        entries,
        total,
        start: q.start_index,
    })
}

/// One tile of a [`query_folded`] listing: a stored item, or a box set
/// standing in for the films of it the listing held.
#[derive(Debug)]
pub enum Folded {
    Item(MediaItem, UserItemData),
    Collection {
        collection: pharos_core::Collection,
        /// How many of the set's films the listing held (two or more).
        members: u32,
        /// The set's film that came first in the listing: the tile stands
        /// where it stood and shows its poster.
        first: MediaItem,
    },
}

impl Folded {
    /// The item whose place in the listing this tile holds.
    pub fn standing_at(&self) -> &MediaItem {
        match self {
            Folded::Item(item, _) => item,
            Folded::Collection { first, .. } => first,
        }
    }
}

/// Fold `items` into box sets (B1086, V1044). A film in several sets goes
/// to the first by name. A set folds only when two or more of its films
/// are in `items`; its tile takes the place of the first of them, and the
/// rest drop out. Every other item keeps its place.
pub async fn fold_into_collections(
    state: &AppState,
    items: Vec<MediaItem>,
) -> Result<Vec<Folded>, DomainError> {
    use pharos_core::CollectionStore as _;
    use std::collections::HashMap;
    let listed: std::collections::HashSet<pharos_core::MediaId> =
        items.iter().map(|i| i.id).collect();
    let mut sets = state.stores.collections_with_counts().await?;
    sets.sort_by(|a, b| a.collection.name.cmp(&b.collection.name));
    // film id -> index into `sets` of the first set by name holding it.
    let mut set_of: HashMap<pharos_core::MediaId, usize> = HashMap::new();
    for (at, set) in sets.iter().enumerate() {
        for id in state
            .stores
            .collection_items(&set.collection.wire_id)
            .await?
        {
            if listed.contains(&id) {
                set_of.entry(id).or_insert(at);
            }
        }
    }
    let mut members: HashMap<usize, u32> = HashMap::new();
    for at in set_of.values() {
        *members.entry(*at).or_default() += 1;
    }
    let mut placed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let folds = set_of
            .get(&item.id)
            .copied()
            .and_then(|at| members.get(&at).filter(|n| **n >= 2).map(|n| (at, *n)));
        match folds {
            None => out.push(Folded::Item(item, UserItemData::default())),
            Some((at, n)) => {
                if placed.insert(at) {
                    out.push(Folded::Collection {
                        collection: sets[at].collection.clone(),
                        members: n,
                        first: item,
                    });
                }
            }
        }
    }
    let sets_folded = placed.len();
    tracing::debug!(
        sets_folded,
        tiles = out.len(),
        "collection fold: listing folded into box sets"
    );
    Ok(out)
}

/// A page of [`query_folded`].
#[derive(Debug)]
pub struct FoldedPage {
    pub entries: Vec<Folded>,
    pub total: u32,
    pub start: u32,
}

/// [`query`] with the stored descriptions, for a caller that reads no
/// overview: it skips the configuration read and translation lookup.
async fn query_unlocalized(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    q: &ItemQuery,
) -> Result<QueryPage, DomainError> {
    use crate::api::jellyfin::items::{
        apply_policy_scope, apply_tags_filter, apply_userdata_filter, build_media_query,
        effective_sort_seed, resolve_parent_filter, shuffle_in_place, sort_primary,
        ParentResolution, SortPrimary,
    };
    let parent = resolve_parent_filter(state, parent_id).await?;
    if matches!(parent, ParentResolution::Empty) {
        return Ok(QueryPage {
            items: Vec::new(),
            total: 0,
            start: q.start_index,
        });
    }
    let is_random = sort_primary(q.sort_by.as_deref()) == SortPrimary::Random;
    let mut mq = build_media_query(user.id, q, &parent, is_random);
    apply_policy_scope(&mut mq, &user.policy, &state.parental_ratings);
    if !is_random {
        let (items, total) = state.stores.query(&mq).await?;
        return Ok(QueryPage {
            items,
            total: u32::try_from(total).unwrap_or(u32::MAX),
            start: q.start_index,
        });
    }
    // The legacy in-memory order: shuffle the kind/search/chip-filtered set,
    // then apply the tag and play-state filters, then page. Only ids move
    // until the page is known; the rows of that page are fetched once.
    let mut ids = state.stores.query_ids(&mq).await?;
    shuffle_in_place(&mut ids, effective_sort_seed(q, user.id));
    let tagged = apply_tags_filter(state, ids, q).await?;
    let filtered = apply_userdata_filter(state, user.id, tagged, q).await?;
    let total = u32::try_from(filtered.len()).unwrap_or(u32::MAX);
    let start = q.start_index as usize;
    let take = q.limit.map_or(usize::MAX, |l| l as usize);
    let page: Vec<pharos_core::MediaId> = filtered.into_iter().skip(start).take(take).collect();
    let items = state.stores.get_many(&page).await?;
    Ok(QueryPage {
        items,
        total,
        start: q.start_index,
    })
}

/// What a listing folds the stored rows under its parent into when it asks
/// for a kind pharos never stores, only synthesises: a TV library's series,
/// a series' seasons, a music library's albums or artists, an artist's
/// albums (V230).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Series,
    Season,
    Album,
    Artist,
}

impl Grouping {
    /// The `shape` label of `pharos_rpc_query_total`.
    pub fn label(self) -> &'static str {
        match self {
            Grouping::Series => "series",
            Grouping::Season => "season",
            Grouping::Album => "album",
            Grouping::Artist => "artist",
        }
    }
}

/// Why [`grouped`] has no page to show.
#[derive(Debug)]
pub enum GroupError {
    /// Seasons were asked for under something that is not a series. Season
    /// tiles are keyed by number alone, so under a library every show's
    /// "Season 1" would fold into one tile.
    SeasonsOutsideSeries,
    /// The store could not answer; carries its error.
    Store(String),
}

/// The synthesised groups under `parent_id`, one page of them as Jellyfin
/// DTOs: the same folding `/Items?IncludeItemTypes=Series` (and `Season`,
/// `MusicAlbum`) and `/Artists` answer with, scoped to what `user` may see.
pub(crate) async fn grouped(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    grouping: Grouping,
    q: &ItemQuery,
) -> Result<crate::api::jellyfin::items::SynthPage, GroupError> {
    use crate::api::jellyfin::items::{
        music_groups_page, resolve_parent_filter, virtual_shows_page, MusicGrouping,
        ParentResolution, ShowFolderKind,
    };
    let store = |e: actix_web::Error| GroupError::Store(e.to_string());
    // The groups are asked for by kind; the stored-kind filter that kind
    // maps to (none) must not also narrow the rows they fold.
    let q = &ItemQuery {
        kinds: KindFilter::Unset,
        ..q.clone()
    };
    let shows = |mode| virtual_shows_page(state, user.id, &user.policy, parent_id, mode, q);
    match grouping {
        Grouping::Series => shows(ShowFolderKind::Series).await.map_err(store),
        Grouping::Season => {
            let parent = resolve_parent_filter(state, parent_id)
                .await
                .map_err(|e| GroupError::Store(e.to_string()))?;
            if !matches!(
                parent,
                ParentResolution::Filter(pharos_core::ParentFilter::Series { .. })
            ) {
                return Err(GroupError::SeasonsOutsideSeries);
            }
            shows(ShowFolderKind::Season).await.map_err(store)
        }
        Grouping::Album => music_groups_page(state, user, parent_id, MusicGrouping::Album, q)
            .await
            .map_err(store),
        Grouping::Artist => music_groups_page(state, user, parent_id, MusicGrouping::Artist, q)
            .await
            .map_err(store),
    }
}

/// What the library filter bar needs beside a listing (B312/V252): where
/// each section of it starts, and the genres there are to pick from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListIndex {
    /// Each section the listing holds and the offset of its first item, in
    /// listing order: first letters in name order (`#` is any name not
    /// starting A to Z), years in a date order (B317/V256; `#` is no date).
    pub letters: Vec<(String, u32)>,
    /// The genres under the parent and kind, by name; the query's own genre
    /// and filters do not narrow it.
    pub genres: Vec<String>,
}

/// What a listing's sections are, by the first key it is sorted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sections {
    /// The first letter of the sort name.
    Letters,
    /// The year of release: the premiere date's, else the production year.
    Released,
    /// The year the item was added.
    Added,
}

impl Sections {
    /// The sections a listing sorted by `sort_by` falls into.
    pub fn of(sort_by: Option<&str>) -> Self {
        use pharos_jellyfin_api::kinds::SortField;
        let first = sort_by.and_then(SortField::first_of);
        match first {
            Some(SortField::PremiereDate | SortField::ProductionYear) => Self::Released,
            Some(SortField::DateCreated | SortField::DateAdded) => Self::Added,
            _ => Self::Letters,
        }
    }
}

/// The letter a sort key files under: its first character upper-cased when
/// that is A to Z, else `#`.
pub fn letter_of(key: &str) -> char {
    match key.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

/// The year in a unix-seconds date, as the rail labels it.
fn year_of(unix_secs: i64) -> String {
    let iso = crate::api::jellyfin::dto::format_iso8601(unix_secs);
    iso.get(..4).unwrap_or("#").to_owned()
}

/// Where each section first appears among `labels` taken in listing order.
/// Walking the real order keeps every start an item of that section
/// whatever collation or null placement the store sorted with.
pub fn section_starts(labels: impl IntoIterator<Item = String>) -> Vec<(String, u32)> {
    let mut seen = std::collections::HashSet::new();
    let mut starts: Vec<(String, u32)> = Vec::new();
    for (at, label) in (0u32..).zip(labels) {
        if seen.insert(label.clone()) {
            starts.push((label, at));
        }
    }
    starts
}

/// The section a stored item is in.
fn item_section(sections: Sections, item: &MediaItem) -> String {
    let year = match sections {
        Sections::Letters => return letter_of(&pharos_core::sort_name(&item.title)).to_string(),
        Sections::Released => item
            .metadata
            .premiere_date
            .map(year_of)
            .or_else(|| item.metadata.production_year.map(|y| y.to_string())),
        Sections::Added => item.created_at.map(year_of),
    };
    year.unwrap_or_else(|| "#".to_owned())
}

/// The section a synthesised group's DTO is in.
fn dto_section(sections: Sections, dto: &serde_json::Value) -> String {
    let iso_year = |field: &str| {
        dto[field]
            .as_str()
            .and_then(|d| d.get(..4))
            .map(str::to_owned)
    };
    let year = match sections {
        Sections::Letters => {
            let name = dto["Name"].as_str().unwrap_or_default();
            return letter_of(&pharos_core::sort_name(name)).to_string();
        }
        Sections::Released => iso_year("PremiereDate")
            .or_else(|| dto["ProductionYear"].as_u64().map(|y| y.to_string())),
        Sections::Added => iso_year("DateCreated"),
    };
    year.unwrap_or_else(|| "#".to_owned())
}

/// The [`ListIndex`] of the listing `q` asks for under `parent_id`: the same
/// listing unpaged, so its letters fall where its pages do.
pub async fn index(
    state: &AppState,
    user: &User,
    parent_id: Option<&str>,
    grouping: Option<Grouping>,
    q: &ItemQuery,
) -> Result<ListIndex, GroupError> {
    use crate::api::jellyfin::items::{
        apply_policy_scope, build_media_query, resolve_parent_filter, ParentResolution,
    };
    let unpaged = ItemQuery {
        start_index: 0,
        limit: None,
        ..q.clone()
    };
    let unfiltered = ItemQuery {
        genre: None,
        filters: None,
        ..unpaged.clone()
    };
    let sections = Sections::of(q.sort_by.as_deref());
    if let Some(grouping) = grouping {
        let group = |q| grouped(state, user, parent_id, grouping, q);
        let listed = group(&unpaged).await?;
        let letters = section_starts(listed.items.iter().map(|dto| dto_section(sections, dto)));
        // Unnarrowed, the listing already is every group there is to pick
        // genres from; folding the episodes again would only repeat it.
        let pool = if q.genre.is_none() && q.filters.is_none() {
            listed.items
        } else {
            group(&unfiltered).await?.items
        };
        let mut genres: Vec<String> = Vec::new();
        for dto in pool {
            for g in dto["Genres"].as_array().into_iter().flatten() {
                if let Some(g) = g.as_str().filter(|g| !g.is_empty()) {
                    if !genres.iter().any(|seen| seen.eq_ignore_ascii_case(g)) {
                        genres.push(g.to_owned());
                    }
                }
            }
        }
        genres.sort_by_key(|g| g.to_lowercase());
        return Ok(ListIndex { letters, genres });
    }
    let store = |e: DomainError| GroupError::Store(e.to_string());
    // Titles and dates only: no description to localize.
    let listed = query_unlocalized(state, user, parent_id, &unpaged)
        .await
        .map_err(store)?;
    let parent = resolve_parent_filter(state, parent_id)
        .await
        .map_err(store)?;
    let genres = if matches!(parent, ParentResolution::Empty) {
        Vec::new()
    } else {
        let mut base = build_media_query(user.id, &unfiltered, &parent, false);
        apply_policy_scope(&mut base, &user.policy, &state.parental_ratings);
        let only_genres = pharos_core::FacetRequest {
            genres: true,
            studios: false,
            tags: false,
            years: false,
            official_ratings: false,
            people: false,
            audio_languages: false,
            subtitle_languages: false,
        };
        let mut genres: Vec<String> = state
            .stores
            .facets(&base, &only_genres)
            .await
            .map_err(store)?
            .genres
            .into_iter()
            .map(|f| f.value)
            .collect();
        genres.sort_by_key(|g| g.to_lowercase());
        genres
    };
    let letters = if q.collapse_collections {
        // A set's tile files under the film it stands at, so the sections
        // fall where the folded pages do.
        let folded = fold_into_collections(state, listed.items)
            .await
            .map_err(store)?;
        section_starts(
            folded
                .iter()
                .map(|e| item_section(sections, e.standing_at())),
        )
    } else {
        section_starts(listed.items.iter().map(|i| item_section(sections, i)))
    };
    Ok(ListIndex { letters, genres })
}

/// `page` with the user's play state for each item, read in one batch.
pub async fn with_user_data(
    state: &AppState,
    user: &User,
    page: QueryPage,
) -> Result<ListPage, DomainError> {
    let ids: Vec<u64> = page.items.iter().map(|i| i.id).collect();
    let user_data = state.stores.user_data_bulk(user.id, &ids).await?;
    Ok(ListPage {
        items: page
            .items
            .into_iter()
            .enumerate()
            .map(|(i, item)| (item, user_data.get(i).copied().unwrap_or_default()))
            .collect(),
        total: page.total,
        start: page.start,
    })
}

/// Why [`detail`] has no item to show.
#[derive(Debug)]
pub enum DetailError {
    /// The user may not see the item, or the store could not say.
    Hidden(crate::services::access::ItemHidden),
    /// No stored item has this id.
    NotFound,
    /// The item or the user's play state could not be read.
    Store(DomainError),
}

/// The stored item `id` with the user's play state, if `user` may see it.
pub async fn detail(
    state: &AppState,
    user: &User,
    id: u64,
) -> Result<(MediaItem, UserItemData), DetailError> {
    crate::services::access::check_item(state, user, id, "item")
        .await
        .map_err(DetailError::Hidden)?;
    let mut item = state.stores.get(id).await.map_err(|e| match e {
        DomainError::NotFound(_) => DetailError::NotFound,
        other => DetailError::Store(other),
    })?;
    // ADR-0062 — the description in the language `user` picked.
    crate::services::metadata_text::localize_items(state, user.id, std::iter::once(&mut item))
        .await;
    let user_data = state
        .stores
        .get_user_data(user.id, id)
        .await
        .map_err(DetailError::Store)?;
    Ok((item, user_data))
}

/// Per-rail sizes the home page uses when the caller names none: the
/// counts jellyfin-web's home sections request (16 latest per library,
/// 12 to resume, 24 next up).
pub const HOME_LATEST_DEFAULT: u32 = 16;
pub const HOME_RESUME_DEFAULT: u32 = 12;
pub const HOME_NEXT_UP_DEFAULT: u32 = 24;

/// The kinds the home page's "Continue watching" row resumes. Music resumes
/// in [`Home::listening`]; a book in neither.
pub const WATCHING_KINDS: [MediaKind; 2] = [MediaKind::Movie, MediaKind::Episode];

/// Everything the home page shows, in one answer: each library the user
/// may see with its newest items, the films and episodes to resume, the
/// music to resume, the books part read, and what is next up.
#[derive(Debug)]
pub struct Home {
    pub rails: Vec<(LibraryView, Vec<LatestTile>)>,
    pub resume: ListPage,
    pub listening: ListPage,
    /// Books part read (FR-012).
    pub reading: ListPage,
    pub next_up: ListPage,
}

/// The home page for `user`, from [`views`], [`latest_tiles`], [`resume`] and
/// [`next_up`]. A limit of `None` takes that rail's `HOME_*_DEFAULT`;
/// `resume_limit` sizes both resume rows.
pub async fn home(
    state: &AppState,
    user: &User,
    latest_per_view: Option<u32>,
    resume_limit: Option<u32>,
    next_up_limit: Option<u32>,
) -> Result<Home, ListError> {
    let latest_n = Some(latest_per_view.unwrap_or(HOME_LATEST_DEFAULT));
    let views = views(state, &user.policy).await;
    let rails = futures_util::future::try_join_all(views.into_iter().map(|view| async move {
        let items = latest_tiles(state, user, Some(&view.id), &KindFilter::Unset, latest_n).await?;
        Ok::<_, ListError>((view, items))
    }));
    let resume_n = Some(resume_limit.unwrap_or(HOME_RESUME_DEFAULT));
    let watching = KindFilter::Kinds(WATCHING_KINDS.to_vec());
    let audio = KindFilter::Kinds(vec![MediaKind::Audio]);
    let listening = resume(state, user, &audio, 0, resume_n);
    let books = KindFilter::Kinds(vec![MediaKind::Book]);
    let reading = resume(state, user, &books, 0, resume_n);
    let resume = resume(state, user, &watching, 0, resume_n);
    let next_up = next_up(
        state,
        user,
        None,
        Some(next_up_limit.unwrap_or(HOME_NEXT_UP_DEFAULT)),
    );
    let (rails, resume, listening, reading, next_up) =
        futures_util::try_join!(rails, resume, listening, reading, next_up)?;
    Ok(Home {
        rails,
        resume,
        listening,
        reading,
        next_up,
    })
}

/// Whether `policy` lets its user see every stored item (every library, no
/// parental cap, no tag list), so no query need narrow what they see.
pub fn hides_nothing(policy: &UserPolicy) -> bool {
    policy.enable_all_folders
        && policy.max_parental_rating.is_none()
        && policy.blocked_tags.is_empty()
        && policy.allowed_tags.is_empty()
}

/// The stored items `user` may see, or `None` when their policy hides
/// nothing (see [`hides_nothing`]).
pub async fn visible_items(
    state: &AppState,
    user: &User,
) -> Result<Option<Vec<MediaItem>>, DomainError> {
    let policy = &user.policy;
    if hides_nothing(policy) {
        return Ok(None);
    }
    let mut mq = pharos_core::MediaQuery::default();
    crate::api::jellyfin::items::apply_policy_scope(&mut mq, policy, &state.parental_ratings);
    let (items, _total) = state.stores.query(&mq).await?;
    Ok(Some(items))
}

/// The stored items whose title, overview or other FTS text matches `term`,
/// ranked by the store, within `visible` when the caller's policy narrows
/// the library (`None` means every stored item). Blank `term` matches
/// nothing. Returns up to `limit`, widened to the visible corpus size when
/// that is larger, so hits outside the user's libraries cannot crowd out
/// the ones inside.
pub async fn search_hits(
    state: &AppState,
    visible: Option<&[MediaItem]>,
    term: &str,
    kinds: Option<Vec<MediaKind>>,
    limit: u32,
) -> Result<Vec<MediaItem>, DomainError> {
    use pharos_core::SearchQuery;
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let fetch = match visible {
        Some(visible) => limit.max(u32::try_from(visible.len()).unwrap_or(u32::MAX)),
        None => limit,
    };
    let (items, _total) = state
        .stores
        .search(&SearchQuery {
            term: term.to_string(),
            kinds: kinds.unwrap_or_default(),
            limit: fetch,
            offset: 0,
        })
        .await?;
    Ok(match visible {
        Some(visible) => {
            let ids: std::collections::HashSet<u64> = visible.iter().map(|i| i.id).collect();
            items.into_iter().filter(|i| ids.contains(&i.id)).collect()
        }
        None => items,
    })
}

/// How many matches a native `Library.search` answers when the caller names
/// no limit: the page the Jellyfin search box shows. The most it answers when
/// one is named is [`QUERY_MAX`], the cap every native listing shares.
pub const SEARCH_DEFAULT: u32 = 25;

/// The stored items `user` may see that match `text`, best match first, at
/// most `limit` (0 is [`SEARCH_DEFAULT`], capped at [`QUERY_MAX`]). The
/// matching and the policy scope are `/Search/Hints`'s, so the two agree on
/// the same term. Blank `text` matches nothing.
pub async fn search(
    state: &AppState,
    user: &User,
    text: &str,
    limit: u32,
) -> Result<Vec<MediaItem>, DomainError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let limit = match limit {
        0 => SEARCH_DEFAULT,
        n => n.min(QUERY_MAX),
    };
    let visible = visible_items(state, user).await?;
    let mut hits = search_hits(state, visible.as_deref(), text, None, limit).await?;
    hits.truncate(limit as usize);
    Ok(hits)
}

/// Whether `/Items?ParentId={parent_wire_id}` lists anything `user` may see.
/// A person, studio, tag or box set has no library of its own: it is
/// visible through its items, and only through them.
pub async fn parent_has_visible_items(
    state: &AppState,
    user: &User,
    parent_wire_id: &str,
) -> Result<bool, DomainError> {
    use crate::api::jellyfin::items::{
        apply_policy_scope, resolve_parent_filter, ParentResolution,
    };
    let mut mq = pharos_core::MediaQuery {
        limit: Some(1),
        ..pharos_core::MediaQuery::default()
    };
    match resolve_parent_filter(state, Some(parent_wire_id)).await? {
        ParentResolution::Empty => return Ok(false),
        ParentResolution::All => {}
        ParentResolution::Filter(pf) => mq.parent = Some(pf),
        ParentResolution::PathPrefix(p) => mq.filters.path_prefix = Some(p),
        ParentResolution::GenreProbe(t) => mq.filters.genre_probe_token = Some(t),
    }
    apply_policy_scope(&mut mq, &user.policy, &state.parental_ratings);
    let (items, total) = state.stores.query(&mq).await?;
    Ok(total > 0 || !items.is_empty())
}

/// A listing of things that have no library of their own (`/Genres`,
/// `/Studios`, `/Tags`, `/Persons`, box sets). The `kind` label of
/// `pharos_entity_hidden_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Genre,
    Studio,
    Tag,
    Person,
    Collection,
}

impl EntityKind {
    pub fn label(self) -> &'static str {
        match self {
            EntityKind::Genre => "genre",
            EntityKind::Studio => "studio",
            EntityKind::Tag => "tag",
            EntityKind::Person => "person",
            EntityKind::Collection => "collection",
        }
    }
}

/// Each entity row with its item count as `user` sees it (V200). For a user
/// whose policy hides nothing the store's count stands and every row is
/// kept; otherwise the count is over `ids_for(wire id)` narrowed to the
/// items they may see, and a row with none is left out. Rows left out for
/// holding only hidden items count under `pharos_entity_hidden_total{kind}`.
pub async fn visible_entity_rows<R, Fut>(
    state: &AppState,
    user: &User,
    kind: EntityKind,
    rows: Vec<R>,
    row: impl Fn(&R) -> (&str, u32),
    ids_for: impl Fn(String) -> Fut,
) -> Result<Vec<(R, u32)>, DomainError>
where
    Fut: std::future::Future<Output = Result<Vec<pharos_core::MediaId>, DomainError>>,
{
    let Some(visible) = visible_items(state, user).await? else {
        return Ok(rows
            .into_iter()
            .map(|r| {
                let count = row(&r).1;
                (r, count)
            })
            .collect());
    };
    let visible: std::collections::HashSet<_> = visible.iter().map(|i| i.id).collect();
    let mut kept = Vec::with_capacity(rows.len());
    let mut hidden: u64 = 0;
    for r in rows {
        let ids = ids_for(row(&r).0.to_string()).await?;
        let count = ids.iter().filter(|id| visible.contains(id)).count();
        match u32::try_from(count).unwrap_or(u32::MAX) {
            0 if ids.is_empty() => {}
            0 => hidden += 1,
            n => kept.push((r, n)),
        }
    }
    if hidden > 0 {
        metrics::counter!("pharos_entity_hidden_total", "kind" => kind.label()).increment(hidden);
        tracing::debug!(user = %user.name, kind = kind.label(), hidden, "entities with no visible item left out");
    }
    Ok(kept)
}

/// What one entity asked for by id shows `user` (V200).
#[derive(Debug)]
pub enum EntityView {
    /// Their policy hides nothing: the store's own count stands.
    All,
    /// The entity's items they may see, at least one.
    Visible(Vec<MediaItem>),
    /// None of its items: answer as an id that names nothing.
    Hidden,
}

/// One genre, person or box set asked for by id (V200). No query for a user whose
/// policy hides nothing; otherwise one `apply_policy_scope` query over the
/// entity's items. An entity whose items are all hidden from `user` counts
/// under `pharos_entity_hidden_total{kind}` and logs a WARN.
pub async fn visible_entity(
    state: &AppState,
    user: &User,
    kind: EntityKind,
    wire_id: &str,
) -> Result<EntityView, DomainError> {
    if hides_nothing(&user.policy) {
        return Ok(EntityView::All);
    }
    let mut mq = pharos_core::MediaQuery::default();
    match kind {
        EntityKind::Genre => mq.genre_wire_id = Some(wire_id.to_string()),
        EntityKind::Studio => mq.studio_wire_id = Some(wire_id.to_string()),
        EntityKind::Tag => mq.tag_wire_ids = vec![wire_id.to_string()],
        EntityKind::Person => mq.person_wire_id = Some(wire_id.to_string()),
        EntityKind::Collection => mq.collection_wire_id = Some(wire_id.to_string()),
    }
    let mut scoped = mq.clone();
    crate::api::jellyfin::items::apply_policy_scope(
        &mut scoped,
        &user.policy,
        &state.parental_ratings,
    );
    let (items, _total) = state.stores.query(&scoped).await?;
    if !items.is_empty() {
        return Ok(EntityView::Visible(items));
    }
    mq.limit = Some(1);
    let (any, _total) = state.stores.query(&mq).await?;
    if !any.is_empty() {
        metrics::counter!("pharos_entity_hidden_total", "kind" => kind.label()).increment(1);
        tracing::warn!(user = %user.name, kind = kind.label(), wire_id, "refused an entity with no visible item");
    }
    Ok(EntityView::Hidden)
}

#[cfg(test)]
mod tests {
    use super::EntityKind;

    #[test]
    fn entity_kind_labels_are_distinct() {
        let labels: std::collections::HashSet<_> = [
            EntityKind::Genre,
            EntityKind::Studio,
            EntityKind::Tag,
            EntityKind::Person,
            EntityKind::Collection,
        ]
        .map(EntityKind::label)
        .into_iter()
        .collect();
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn latest_tile_labels_are_distinct_and_stable() {
        let labels = super::TileKind::ALL.map(super::TileKind::label);
        assert_eq!(labels, ["item", "series", "album"]);
    }

    #[test]
    fn next_up_outcome_labels_are_distinct_and_stable() {
        let labels = super::NextUpOutcome::ALL.map(super::NextUpOutcome::label);
        assert_eq!(labels, ["listed", "empty", "error"]);
    }

    #[test]
    fn letters_start_where_their_first_name_is() {
        let names = [
            "2001",
            "(500) Days",
            "Alpha",
            "The Beta",
            "Bravo",
            "Émile",
            "Zulu",
        ];
        assert_eq!(
            letter_starts(names),
            starts(&[("#", 0), ("A", 2), ("B", 3), ("Z", 6)])
        );
    }

    #[test]
    fn a_leading_article_files_under_the_next_word() {
        assert_eq!(letter_starts(["The Glass Lattice"]), starts(&[("G", 0)]));
    }

    #[test]
    fn date_sections_are_years_in_listing_order() {
        let years = ["2024", "2024", "2019", "#", "#"].map(String::from);
        assert_eq!(
            super::section_starts(years),
            starts(&[("2024", 0), ("2019", 2), ("#", 3)])
        );
    }

    #[test]
    fn the_first_sort_key_picks_the_sections() {
        use super::Sections;
        assert!(matches!(Sections::of(None), Sections::Letters));
        assert!(matches!(Sections::of(Some("SortName")), Sections::Letters));
        assert!(matches!(
            Sections::of(Some("PremiereDate,ProductionYear,SortName")),
            Sections::Released
        ));
        assert!(matches!(
            Sections::of(Some("DateCreated,SortName")),
            Sections::Added
        ));
    }

    /// Sections in name order, labelled as `item_section` labels a title.
    fn letter_starts<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<(String, u32)> {
        super::section_starts(
            names
                .into_iter()
                .map(|name| super::letter_of(&pharos_core::sort_name(name)).to_string()),
        )
    }

    fn starts(want: &[(&str, u32)]) -> Vec<(String, u32)> {
        want.iter().map(|&(l, i)| (l.to_owned(), i)).collect()
    }
}
