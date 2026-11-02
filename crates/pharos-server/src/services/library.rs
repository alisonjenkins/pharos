//! The library: what a user may browse (spec 017 US4). Both wire layers list
//! libraries through here, so the Jellyfin views and `Library.views` cannot
//! drift apart.

use pharos_core::{
    DomainError, LibraryKind, MediaItem, MediaKind, User, UserDataStore as _, UserItemData,
    UserPolicy,
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
/// `IncludeItemTypes=`; the native API from a list of kinds.
///
/// B153 — the distinction that was missing. `MusicVideo` names a type pharos
/// stores none of, and the old code read "zero recognised kinds" as "no kind
/// filter", so a request for music videos returned the ENTIRE library: 51.6 MB
/// of JSON for a query whose honest answer is an empty list. "I asked for X"
/// must never widen to "I asked for anything".
///
/// Reaching this point already means every synth short-circuit (MusicAlbum,
/// MusicArtist, Playlist, BoxSet, the show folders) has declined the request,
/// and the SQL path can only ever return Movie / Episode / Audio rows — so if
/// none of the named types is one of those, nothing can match, whichever
/// unknown type it was.
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

/// Why a "latest" listing could not be built.
#[derive(Debug)]
pub enum LatestError {
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
) -> Result<Vec<(MediaItem, UserItemData)>, LatestError> {
    let all = state
        .list_items_cached()
        .await
        .map_err(LatestError::Store)?;
    let all = crate::services::access::visible_items(state, user, all, "latest")
        .await
        .map_err(LatestError::Hidden)?;
    let scoped = crate::api::jellyfin::items::restrict_to_parent(state, &all, parent_id).await;
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
        .map_err(LatestError::Store)?;
    Ok(typed
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            (item, ud)
        })
        .collect())
}
