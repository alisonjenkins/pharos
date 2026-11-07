//! The library: what a user may browse (spec 017 US4). Both wire layers list
//! libraries through here, so the Jellyfin views and `Library.views` cannot
//! drift apart.

use pharos_core::{
    DomainError, LibraryKind, MediaItem, MediaKind, MediaStore as _, User, UserDataStore as _,
    UserItemData, UserPolicy,
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
    let all = state.list_items_cached().await.map_err(ListError::Store)?;
    let all = crate::services::access::visible_items(state, user, all, "latest")
        .await
        .map_err(ListError::Hidden)?;
    // Home-page "Latest" rows must match the library the user clicked into,
    // and jellyfin-web's "Latest Movies" row filters to Type=Movie.
    let scoped = restrict_to_parent(state, &all, parent_id).await;
    let mut typed = kinds.keep(scoped);
    // B97 — "Latest" means most-recently-ADDED first (DateCreated desc), so a
    // newly scanned film surfaces in the home "Recently Added" row. Rows
    // lacking a timestamp sort last; id breaks ties so the order is stable.
    typed.sort_by(|a, b| {
        b.created_at
            .unwrap_or(i64::MIN)
            .cmp(&a.created_at.unwrap_or(i64::MIN))
            .then(b.id.cmp(&a.id))
    });
    typed.truncate(limit.unwrap_or(LATEST_MAX).min(LATEST_MAX) as usize);
    crate::services::metadata_text::localize_items(state, user.id, typed.iter_mut()).await;
    let ids: Vec<u64> = typed.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(ListError::Store)?;
    Ok(typed
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            (item, ud)
        })
        .collect())
}

/// One page of a listing, with the number of matching items before paging.
#[derive(Debug)]
pub struct ListPage {
    pub items: Vec<(MediaItem, UserItemData)>,
    pub total: u32,
    pub start: u32,
}

/// The items `user` has started and not finished, of the kinds `kinds`
/// keeps, in the store's resume order, each with the user's play state.
/// `start` and `limit` page the list; `total` counts every match (B164).
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
    crate::services::access::retain_visible(state, user, &mut ids, |id| *id, "resume")
        .await
        .map_err(ListError::Hidden)?;
    let mut items: Vec<MediaItem> = Vec::with_capacity(ids.len());
    for id in &ids {
        match state.stores.get(*id).await {
            Ok(item) => items.push(item),
            // Deleted since it was resumed: skipped, not an error.
            Err(DomainError::NotFound(_)) => {}
            Err(e) => return Err(ListError::Store(e)),
        }
    }
    let items = kinds.keep(items);
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

/// For each series `user` may see (or only `series_id`, a series wire id),
/// the lowest-numbered episode the user has not played, ordered by series
/// name, each with the user's play state. At most `limit` (at least 1);
/// `None` means [`NEXT_UP_DEFAULT`].
pub async fn next_up(
    state: &AppState,
    user: &User,
    series_id: Option<&str>,
    limit: Option<u32>,
) -> Result<ListPage, ListError> {
    let all = state.list_items_cached().await.map_err(ListError::Store)?;
    let all = crate::services::access::visible_items(state, user, all, "next_up")
        .await
        .map_err(ListError::Hidden)?;
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(ListError::Store)?;
    // Group episodes by series; pick the lowest unwatched per series.
    use crate::api::jellyfin::dto::series_id_for_key;
    use std::collections::HashMap;
    // B92 — the kotlin SDK sends `?SeriesId=` dashed; canonicalise once.
    let series_id_want =
        series_id.map(|s| crate::api::jellyfin::items::canonical_wire_id(s).into_owned());
    let mut buckets: HashMap<String, Vec<(usize, &MediaItem)>> = HashMap::new();
    for (idx, item) in all.iter().enumerate() {
        if !matches!(item.kind, MediaKind::Episode) {
            continue;
        }
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        // A Series-page request scopes to that series (`SeriesId`); the
        // home-screen row leaves it unset and gets one pick per series.
        // B92 — canonicalise the dashed `SeriesId` the Android TV Series page
        // sends, else Next Up on that page comes back empty.
        if let Some(want) = series_id_want.as_deref() {
            if series_id_for_key(series.series_folder.as_deref(), &series.series_name).as_str()
                != want
            {
                continue;
            }
        }
        // Skip already-played episodes.
        if user_data.get(idx).copied().unwrap_or_default().played {
            continue;
        }
        // LIB-C11 — bucket by the folder-keyed identity, not the bare
        // name, so two same-name shows in distinct folders stay separate
        // and don't interleave into one NextUp pick.
        buckets
            .entry(series.series_key().to_string())
            .or_default()
            .push((idx, item));
    }
    // Sort each bucket by (season_number, episode_number) ascending,
    // pick the head.
    let mut picks: Vec<(usize, &MediaItem)> = buckets
        .into_values()
        .filter_map(|mut eps| {
            eps.sort_by_key(|(_, e)| {
                let s = e.series.as_ref().and_then(|s| s.season_number).unwrap_or(0);
                let n = e
                    .series
                    .as_ref()
                    .and_then(|s| s.episode_number)
                    .unwrap_or(0);
                (s, n)
            });
            eps.into_iter().next()
        })
        .collect();
    // Stable series-name sort across the result set.
    picks.sort_by(|a, b| {
        let an =
            a.1.series
                .as_ref()
                .map(|s| s.series_name.as_str())
                .unwrap_or("");
        let bn =
            b.1.series
                .as_ref()
                .map(|s| s.series_name.as_str())
                .unwrap_or("");
        an.cmp(bn)
    });
    picks.truncate(limit.unwrap_or(NEXT_UP_DEFAULT).max(1) as usize);
    let total = u32::try_from(picks.len()).unwrap_or(u32::MAX);
    let mut items: Vec<(MediaItem, UserItemData)> = picks
        .into_iter()
        .map(|(idx, item)| {
            (
                item.clone(),
                user_data.get(idx).copied().unwrap_or_default(),
            )
        })
        .collect();
    crate::services::metadata_text::localize_items(
        state,
        user.id,
        items.iter_mut().map(|(i, _)| i),
    )
    .await;
    Ok(ListPage {
        items,
        total,
        start: 0,
    })
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
            .is_some_and(|s| series_id_for_key(s.series_folder.as_deref(), &s.series_name) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    series_id_for_key(s.series_folder.as_deref(), &s.series_name) == pid
                })
            })
            .cloned()
            .collect();
    }
    // 3) Season id → every episode in that (folder, season) pair.
    if items.iter().any(|i| {
        i.series.as_ref().is_some_and(|s| {
            s.season_number.is_some_and(|n| {
                season_id_for_key(s.series_folder.as_deref(), &s.series_name, n) == pid
            })
        })
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    s.season_number.is_some_and(|n| {
                        season_id_for_key(s.series_folder.as_deref(), &s.series_name, n) == pid
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
    // then apply the tag and play-state filters, then page.
    let (mut items, _total) = state.stores.query(&mq).await?;
    shuffle_in_place(&mut items, effective_sort_seed(q, user.id));
    let tagged = apply_tags_filter(state, items, q).await?;
    let filtered = apply_userdata_filter(state, user.id, tagged, q).await?;
    let total = u32::try_from(filtered.len()).unwrap_or(u32::MAX);
    let start = q.start_index as usize;
    let take = q.limit.map_or(usize::MAX, |l| l as usize);
    let items: Vec<MediaItem> = filtered.into_iter().skip(start).take(take).collect();
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
    Ok(ListIndex {
        letters: section_starts(listed.items.iter().map(|i| item_section(sections, i))),
        genres,
    })
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
/// music to resume, and what is next up.
#[derive(Debug)]
pub struct Home {
    pub rails: Vec<(LibraryView, Vec<(MediaItem, UserItemData)>)>,
    pub resume: ListPage,
    pub listening: ListPage,
    pub next_up: ListPage,
}

/// The home page for `user`, from [`views`], [`latest`], [`resume`] and
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
        let items = latest(state, user, Some(&view.id), &KindFilter::Unset, latest_n).await?;
        Ok::<_, ListError>((view, items))
    }));
    let resume_n = Some(resume_limit.unwrap_or(HOME_RESUME_DEFAULT));
    let watching = KindFilter::Kinds(WATCHING_KINDS.to_vec());
    let audio = KindFilter::Kinds(vec![MediaKind::Audio]);
    let listening = resume(state, user, &audio, 0, resume_n);
    let resume = resume(state, user, &watching, 0, resume_n);
    let next_up = next_up(
        state,
        user,
        None,
        Some(next_up_limit.unwrap_or(HOME_NEXT_UP_DEFAULT)),
    );
    let (rails, resume, listening, next_up) =
        futures_util::try_join!(rails, resume, listening, next_up)?;
    Ok(Home {
        rails,
        resume,
        listening,
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
        assert_eq!(letter_starts(["The Matrix"]), starts(&[("M", 0)]));
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
