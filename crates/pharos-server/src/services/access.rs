//! Item access: whether a user's library policy lets them reach one item
//! (V187). Every by-id route asks here before it serves an item, so the
//! Jellyfin routes and the native API answer alike and a new route cannot
//! forget the check by re-implementing it.

use std::sync::Arc;

use pharos_core::{LibraryStore, MediaId, MediaItem, User};

use crate::state::AppState;

/// Why an item was withheld. A wire layer answers `NotAllowed` as "not
/// found", so a hidden item is indistinguishable from a missing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemHidden {
    /// The item's library is not in the user's `EnabledFolders`.
    LibraryNotAllowed,
    /// The item belongs to no library, so no allow-list can admit it.
    NoLibrary,
    /// The store could not say; the item is withheld rather than served.
    Unavailable(String),
}

impl ItemHidden {
    /// The `reason` label of `pharos_item_access_denied_total`.
    pub fn label(&self) -> &'static str {
        match self {
            ItemHidden::LibraryNotAllowed => "library_not_allowed",
            ItemHidden::NoLibrary => "no_library",
            ItemHidden::Unavailable(_) => "store_unavailable",
        }
    }
}

impl std::fmt::Display for ItemHidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemHidden::Unavailable(detail) => {
                write!(f, "item access: store unavailable: {detail}")
            }
            other => f.write_str(other.label()),
        }
    }
}

/// `Ok` when `user` may reach item `id`. An unrestricted policy costs no
/// store call. Each refusal counts under
/// `pharos_item_access_denied_total{reason}` and logs the item, the user,
/// the library it lives in and `route`, the class of route that asked
/// (`"item"`, `"stream"`, `"hls"`, …).
pub async fn check_item(
    state: &AppState,
    user: &User,
    id: MediaId,
    route: &'static str,
) -> Result<(), ItemHidden> {
    let policy = &user.policy;
    if policy.enable_all_folders {
        return Ok(());
    }
    let (verdict, library) = match state.stores.library_wire_id_of(id).await {
        Ok(Some(library)) if policy.allows_library(&library) => return Ok(()),
        Ok(Some(library)) => (ItemHidden::LibraryNotAllowed, Some(library)),
        Ok(None) => (ItemHidden::NoLibrary, None),
        Err(e) => (ItemHidden::Unavailable(e.to_string()), None),
    };
    metrics::counter!("pharos_item_access_denied_total", "reason" => verdict.label()).increment(1);
    tracing::warn!(
        item_id = id,
        user = %user.name,
        library = library.as_deref().unwrap_or(""),
        reason = verdict.label(),
        route,
        detail = %verdict,
        "item withheld by library policy"
    );
    Err(verdict)
}

/// Keeps the entries of `items` whose item `user` may reach, `id` naming
/// each entry's item. For lists built outside a `MediaQuery` (similar items,
/// instant mixes, playlist entries, a series' episodes), which
/// `apply_policy_scope` never sees. One store call per allowed library for a
/// restricted user, none otherwise. The dropped count is logged, not each
/// item: a list is not a request for any one of them.
pub async fn retain_visible<T>(
    state: &AppState,
    user: &User,
    items: &mut Vec<T>,
    id: impl Fn(&T) -> MediaId,
    route: &'static str,
) -> Result<(), ItemHidden> {
    let policy = &user.policy;
    if policy.enable_all_folders {
        return Ok(());
    }
    let mut allowed = std::collections::HashSet::new();
    for library in &policy.enabled_folders {
        let ids = state
            .stores
            .item_ids_for_library(library)
            .await
            .map_err(|e| {
                let hidden = ItemHidden::Unavailable(e.to_string());
                metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label())
                    .increment(1);
                tracing::warn!(user = %user.name, route, detail = %hidden, "item list withheld");
                hidden
            })?;
        allowed.extend(ids);
    }
    let before = items.len();
    items.retain(|item| allowed.contains(&id(item)));
    let dropped = before - items.len();
    if dropped > 0 {
        tracing::debug!(user = %user.name, route, dropped, "items outside the user's libraries left out");
    }
    Ok(())
}

/// The cached item list narrowed to what `user` may reach. Routes that
/// build a list or an aggregate (latest, next up, artists, albums, genres,
/// counts) straight from `list_items_cached` filter through this, since no
/// `MediaQuery` scopes them. An unrestricted user gets `all` back as is.
pub async fn visible_items(
    state: &AppState,
    user: &User,
    all: Arc<Vec<MediaItem>>,
    route: &'static str,
) -> Result<Arc<Vec<MediaItem>>, ItemHidden> {
    if user.policy.enable_all_folders {
        return Ok(all);
    }
    let mut kept: Vec<&MediaItem> = all.iter().collect();
    retain_visible(state, user, &mut kept, |i| i.id, route).await?;
    Ok(Arc::new(kept.into_iter().cloned().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_labels_are_distinct() {
        let labels = [
            ItemHidden::LibraryNotAllowed.label(),
            ItemHidden::NoLibrary.label(),
            ItemHidden::Unavailable(String::new()).label(),
        ];
        let unique: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
