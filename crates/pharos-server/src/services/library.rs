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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindFilter {
    /// No kind named: every kind is acceptable.
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
        // A row deleted since it was resumed is skipped, not an error.
        if let Ok(item) = state.stores.get(*id).await {
            items.push(item);
        }
    }
    let items = kinds.keep(items);
    let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
    let start = start.min(total);
    let end = limit.map_or(total, |n| start.saturating_add(n)).min(total);
    let page: Vec<MediaItem> = items
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect();
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
