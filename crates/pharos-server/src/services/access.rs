//! Item access: whether a user's policy (libraries, parental cap, tag lists)
//! lets them reach one item (V187). Every by-id route asks here before it serves an item, so the
//! Jellyfin routes and the native API answer alike and a new route cannot
//! forget the check by re-implementing it.

use std::sync::Arc;

use pharos_core::{LibraryStore, MediaFilters, MediaId, MediaItem, MediaQuery, MediaStore, User};

use crate::services::library::hides_nothing;
use crate::state::AppState;

/// Ids per policy query: well under SQLite's bound-parameter limit once the
/// policy's own parameters are added.
const IDS_PER_QUERY: usize = 500;

/// Why an item was withheld. A wire layer answers `NotAllowed` as "not
/// found", so a hidden item is indistinguishable from a missing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemHidden {
    /// The item's library is not in the user's `EnabledFolders`.
    LibraryNotAllowed,
    /// The item belongs to no library, so no allow-list can admit it.
    NoLibrary,
    /// The item's rating is above the user's `MaxParentalRating`, or it is
    /// unrated and the user's unrated items are blocked.
    ParentalRating,
    /// The item carries one of the user's `BlockedTags`, or none of their
    /// `AllowedTags`.
    Tag,
    /// No stored item has the id, so the policy could not be checked.
    NotFound,
    /// The store could not say; the item is withheld rather than served.
    Unavailable(String),
    /// The request carried no credential, or one that resolves to no
    /// enabled user. Answered 401, not 404: the caller is not yet anyone.
    Unauthenticated,
    /// The credential is live but its scope (a cast TV's playback token) does
    /// not permit this route. Answered 403.
    OutOfScope,
}

impl ItemHidden {
    /// The `reason` label of `pharos_item_access_denied_total`.
    pub fn label(&self) -> &'static str {
        match self {
            ItemHidden::LibraryNotAllowed => "library_not_allowed",
            ItemHidden::NoLibrary => "no_library",
            ItemHidden::ParentalRating => "parental_rating",
            ItemHidden::Tag => "tag",
            ItemHidden::NotFound => "not_found",
            ItemHidden::Unavailable(_) => "store_unavailable",
            ItemHidden::Unauthenticated => "unauthenticated",
            ItemHidden::OutOfScope => "scope_restricted",
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
    if hides_nothing(policy) {
        return Ok(());
    }
    let library = if policy.enable_all_folders {
        None
    } else {
        match state.stores.library_wire_id_of(id).await {
            Ok(Some(library)) if policy.allows_library(&library) => Some(library),
            Ok(Some(library)) => {
                return Err(refused(
                    user,
                    id,
                    route,
                    ItemHidden::LibraryNotAllowed,
                    Some(&library),
                ))
            }
            Ok(None) => return Err(refused(user, id, route, ItemHidden::NoLibrary, None)),
            Err(e) => {
                return Err(refused(
                    user,
                    id,
                    route,
                    ItemHidden::Unavailable(e.to_string()),
                    None,
                ))
            }
        }
    };
    match rating_or_tag(state, user, id).await {
        None => Ok(()),
        Some(verdict) => Err(refused(user, id, route, verdict, library.as_deref())),
    }
}

/// Count and log `verdict` for item `id`, and pass it on.
fn refused(
    user: &User,
    id: MediaId,
    route: &'static str,
    verdict: ItemHidden,
    library: Option<&str>,
) -> ItemHidden {
    metrics::counter!("pharos_item_access_denied_total", "reason" => verdict.label()).increment(1);
    tracing::warn!(
        item_id = id,
        user = %user.name,
        library = library.unwrap_or(""),
        reason = verdict.label(),
        route,
        detail = %verdict,
        "item withheld by policy"
    );
    verdict
}

/// Why the parental cap or a tag list hides item `id` from `user`, or `None`
/// when neither does. An item the full policy does not show is always
/// withheld; the probes with the tag lists, then the cap, lifted only name
/// the reason, and an item no probe shows is `NotFound`.
async fn rating_or_tag(state: &AppState, user: &User, id: MediaId) -> Option<ItemHidden> {
    let policy = &user.policy;
    if policy.max_parental_rating.is_none()
        && policy.blocked_tags.is_empty()
        && policy.allowed_tags.is_empty()
    {
        return None;
    }
    let shown = match policy_visible_ids(state, user, &[id]).await {
        Ok(shown) => shown,
        Err(hidden) => return Some(hidden),
    };
    if shown.contains(&id) {
        return None;
    }
    // Ask again with the tag lists lifted: if the item shows, a tag hid it.
    let mut lifted = user.clone();
    lifted.policy.blocked_tags.clear();
    lifted.policy.allowed_tags.clear();
    match policy_visible_ids(state, &lifted, &[id]).await {
        Ok(shown) if shown.contains(&id) => return Some(ItemHidden::Tag),
        Ok(_) => {}
        Err(hidden) => return Some(hidden),
    }
    lifted.policy.max_parental_rating = None;
    Some(match policy_visible_ids(state, &lifted, &[id]).await {
        Ok(shown) if shown.contains(&id) => ItemHidden::ParentalRating,
        Ok(_) => ItemHidden::NotFound,
        Err(hidden) => hidden,
    })
}

/// The ids among `ids` that `user`'s policy shows, asked of the store under
/// the same `apply_policy_scope` as `/Items`, so the two cannot disagree.
async fn policy_visible_ids(
    state: &AppState,
    user: &User,
    ids: &[MediaId],
) -> Result<std::collections::HashSet<MediaId>, ItemHidden> {
    let mut shown = std::collections::HashSet::new();
    for chunk in ids.chunks(IDS_PER_QUERY) {
        let mut mq = MediaQuery {
            filters: MediaFilters {
                ids: chunk.to_vec(),
                ids_present: true,
                ..MediaFilters::default()
            },
            ..MediaQuery::default()
        };
        crate::api::jellyfin::items::apply_policy_scope(
            &mut mq,
            &user.policy,
            &state.parental_ratings,
        );
        let visible = state
            .stores
            .query_ids(&mq)
            .await
            .map_err(|e| ItemHidden::Unavailable(e.to_string()))?;
        shown.extend(visible);
    }
    Ok(shown)
}

/// The user behind `token` on a route that serves an item's files to
/// element URLs (images, subtitles, attachments), which carry the token in
/// the query or a cookie rather than a header. A missing token, or one that
/// resolves to no enabled user, counts under
/// `pharos_item_access_denied_total{reason="unauthenticated"}` and logs the
/// item asked for (`item`, as the path spelled it) and `route`: at DEBUG for
/// a missing token, WARN for one that does not resolve. A restricted token
/// (a cast TV's) is checked against `method` on the matched route `pattern`
/// and refused with [`ItemHidden::OutOfScope`] where its scope does not reach
/// (V225).
pub async fn authenticate(
    state: &AppState,
    token: Option<&str>,
    item: &str,
    route: &'static str,
    method: &str,
    pattern: Option<&str>,
) -> Result<User, ItemHidden> {
    let refusal = match token {
        None => "missing",
        Some(token) => {
            match crate::services::auth::scoped_user_for_token(&state.stores, token).await {
                Ok((user, scope)) => {
                    if crate::api::jellyfin::auth_extractor::scope_refusal(
                        scope, user.id, method, pattern,
                    )
                    .is_some()
                    {
                        let hidden = ItemHidden::OutOfScope;
                        metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label())
                        .increment(1);
                        return Err(hidden);
                    }
                    return Ok(user);
                }
                Err(crate::services::auth::TokenRefusal::Unavailable(detail)) => {
                    let hidden = ItemHidden::Unavailable(detail);
                    metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label())
                    .increment(1);
                    tracing::warn!(item_id = item, route, reason = hidden.label(), detail = %hidden, "item withheld: credential not checked");
                    return Err(hidden);
                }
                Err(refusal) => refusal.label(),
            }
        }
    };
    let hidden = ItemHidden::Unauthenticated;
    metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label()).increment(1);
    // A missing credential is what every anonymous crawler sends to these
    // internet-facing routes; the counter keeps its rate visible without a
    // WARN per request. A presented token that does not resolve stays WARN.
    if token.is_none() {
        tracing::debug!(
            item_id = item,
            route,
            reason = hidden.label(),
            credential = refusal,
            "item withheld from an unauthenticated request"
        );
    } else {
        tracing::warn!(
            item_id = item,
            route,
            reason = hidden.label(),
            credential = refusal,
            "item withheld from an unauthenticated request"
        );
    }
    Err(hidden)
}

/// Keeps the entries of `items` whose item `user` may reach, `id` naming
/// each entry's item. For lists built outside a `MediaQuery` (similar items,
/// instant mixes, playlist entries, a series' episodes), which
/// `apply_policy_scope` never sees (see [`allowed_ids`] for the store calls;
/// none for an unrestricted user). The
/// dropped count is logged, not each item: a list is not a request for any
/// one of them.
pub async fn retain_visible<T>(
    state: &AppState,
    user: &User,
    items: &mut Vec<T>,
    id: impl Fn(&T) -> MediaId,
    route: &'static str,
) -> Result<(), ItemHidden> {
    if hides_nothing(&user.policy) {
        return Ok(());
    }
    let ids: Vec<MediaId> = items.iter().map(&id).collect();
    let allowed = allowed_ids(state, user, &ids)
        .await
        .map_err(|hidden| list_withheld(user, route, hidden))?;
    let before = items.len();
    items.retain(|item| allowed.contains(&id(item)));
    let dropped = before - items.len();
    if dropped > 0 {
        tracing::debug!(user = %user.name, route, dropped, "items the user's policy hides left out");
    }
    Ok(())
}

/// The ids among `ids` that `user` may reach. A policy that restricts only
/// libraries reads each allowed library's ids; one with a parental cap or a
/// tag list asks the scoped query, by id for a short list and for the whole
/// visible library for a long one.
async fn allowed_ids(
    state: &AppState,
    user: &User,
    ids: &[MediaId],
) -> Result<std::collections::HashSet<MediaId>, ItemHidden> {
    let policy = &user.policy;
    if policy.max_parental_rating.is_none()
        && policy.blocked_tags.is_empty()
        && policy.allowed_tags.is_empty()
    {
        let mut allowed = std::collections::HashSet::new();
        for library in &policy.enabled_folders {
            let library_ids = state
                .stores
                .item_ids_for_library(library)
                .await
                .map_err(|e| ItemHidden::Unavailable(e.to_string()))?;
            allowed.extend(library_ids);
        }
        return Ok(allowed);
    }
    if ids.len() <= IDS_PER_QUERY {
        return policy_visible_ids(state, user, ids).await;
    }
    let mut mq = MediaQuery::default();
    crate::api::jellyfin::items::apply_policy_scope(&mut mq, policy, &state.parental_ratings);
    let visible = state
        .stores
        .query_ids(&mq)
        .await
        .map_err(|e| ItemHidden::Unavailable(e.to_string()))?;
    Ok(visible.into_iter().collect())
}

/// Count and log a list the store could not scope, and pass `hidden` on.
fn list_withheld(user: &User, route: &'static str, hidden: ItemHidden) -> ItemHidden {
    metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label()).increment(1);
    tracing::warn!(user = %user.name, route, detail = %hidden, "item list withheld");
    hidden
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
    if hides_nothing(&user.policy) {
        return Ok(all);
    }
    let ids: Vec<MediaId> = all.iter().map(|i| i.id).collect();
    let allowed = allowed_ids(state, user, &ids)
        .await
        .map_err(|hidden| list_withheld(user, route, hidden))?;
    Ok(Arc::new(
        all.iter()
            .filter(|i| allowed.contains(&i.id))
            .cloned()
            .collect(),
    ))
}

/// What a caller wants of a playlist: `Read` lists it, `Write` changes or
/// deletes it. The `action` label of `pharos_playlist_access_denied_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistAccess {
    Read,
    Write,
}

impl PlaylistAccess {
    pub fn label(self) -> &'static str {
        match self {
            PlaylistAccess::Read => "read",
            PlaylistAccess::Write => "write",
        }
    }
}

/// Whether `user` may `access` `playlist`. A playlist answers its owner; a
/// server-owned one (no owner), or one whose owner has been deleted, is
/// read by everyone and changed only by an admin. A wire layer answers a
/// refusal as "not found", so another user's playlist is indistinguishable
/// from a missing one. Each refusal counts under
/// `pharos_playlist_access_denied_total{action}`.
pub async fn check_playlist(
    state: &AppState,
    user: &User,
    playlist: &pharos_core::Playlist,
    access: PlaylistAccess,
    route: &'static str,
) -> bool {
    let server_owned = access == PlaylistAccess::Read || user.policy.admin;
    let allowed = match playlist.owner_user_id.as_deref() {
        Some(owner) if owner == user.id.0.simple().to_string() => true,
        Some(owner) => server_owned && owner_is_gone(state, owner).await,
        None => server_owned,
    };
    if !allowed {
        metrics::counter!("pharos_playlist_access_denied_total", "action" => access.label())
            .increment(1);
        tracing::warn!(
            playlist = %playlist.wire_id,
            owner = playlist.owner_user_id.as_deref().unwrap_or("server"),
            user = %user.name,
            action = access.label(),
            route,
            "playlist withheld from a user who does not own it"
        );
    }
    allowed
}

/// Whether a playlist's recorded owner no longer names a user. A store
/// failure answers `false`, so the playlist stays its owner's.
async fn owner_is_gone(state: &AppState, owner: &str) -> bool {
    let Ok(id) = uuid::Uuid::parse_str(owner) else {
        return true;
    };
    match pharos_core::UserStore::get(&state.stores, pharos_core::UserId(id)).await {
        Err(pharos_core::AuthError::UserNotFound) => true,
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(owner, error = %e, "playlist owner lookup failed; playlist withheld");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_action_labels_are_distinct() {
        assert_ne!(PlaylistAccess::Read.label(), PlaylistAccess::Write.label());
    }

    #[test]
    fn reason_labels_are_distinct() {
        let labels = [
            ItemHidden::LibraryNotAllowed.label(),
            ItemHidden::NoLibrary.label(),
            ItemHidden::ParentalRating.label(),
            ItemHidden::Tag.label(),
            ItemHidden::NotFound.label(),
            ItemHidden::Unavailable(String::new()).label(),
            ItemHidden::Unauthenticated.label(),
        ];
        let unique: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
