//! A user's own state on an item: played and favourite (spec 017 US5). Both
//! wire layers write it here, so marking an item from either API checks the
//! same policy and tells every connected jellyfin-web the same thing.

use pharos_core::{DomainError, MediaItem, MediaStore, User, UserDataStore, UserItemData};

use crate::api::jellyfin::dto::UserItemDataDto;
use crate::services::library::DetailError;
use crate::state::AppState;

/// Marks `d` played, counting the play, or unplayed. Unplayed also forgets
/// the resume position: Jellyfin's `MarkUnplayed` clears both, and an item
/// that says "unwatched" and then starts 40 minutes in contradicts itself
/// (B176).
pub fn apply_played(d: &mut UserItemData, played: bool) {
    if played {
        d.played = true;
        d.play_count = d.play_count.saturating_add(1);
        d.last_played_position_ticks = 0;
        d.last_played_at = crate::sync_recovery::now_unix_secs();
    } else {
        d.played = false;
        d.last_played_position_ticks = 0;
    }
}

pub fn apply_favorite(d: &mut UserItemData, favorite: bool) {
    d.is_favorite = favorite;
}

/// Sets whether `user` has played stored item `id`.
pub async fn set_played(
    state: &AppState,
    user: &User,
    id: u64,
    played: bool,
) -> Result<(MediaItem, UserItemData), DetailError> {
    update(state, user, id, |d| apply_played(d, played)).await
}

/// Sets whether stored item `id` is one of `user`'s favourites.
pub async fn set_favorite(
    state: &AppState,
    user: &User,
    id: u64,
    favorite: bool,
) -> Result<(MediaItem, UserItemData), DetailError> {
    update(state, user, id, |d| apply_favorite(d, favorite)).await
}

/// Applies `change` to `user`'s state on stored item `id`, if they may see
/// it, and announces the new state to their connected sockets. Answers the
/// item and the state as written.
pub async fn update(
    state: &AppState,
    user: &User,
    id: u64,
    change: impl FnOnce(&mut UserItemData),
) -> Result<(MediaItem, UserItemData), DetailError> {
    crate::services::access::check_item(state, user, id, "user_data")
        .await
        .map_err(DetailError::Hidden)?;
    // The item must exist before a row is written that the cascade would
    // have to clean up later.
    let item = state.stores.get(id).await.map_err(|e| match e {
        DomainError::NotFound(_) => DetailError::NotFound,
        other => DetailError::Store(other),
    })?;
    let mut data = state
        .stores
        .get_user_data(user.id, id)
        .await
        .map_err(DetailError::Store)?;
    change(&mut data);
    state
        .stores
        .set_user_data(user.id, id, data)
        .await
        .map_err(DetailError::Store)?;
    let runtime = item.probe.run_time_ticks().unwrap_or(0);
    let dto = UserItemDataDto::from_domain_with_runtime(id, data, runtime);
    // The full DTO, so jellyfin-web updates the watched mark and the
    // favourite star in place (B36).
    match serde_json::to_value(&dto) {
        Ok(entry) => state.notify_user_data_changed(&user.id.0.simple().to_string(), vec![entry]),
        Err(e) => tracing::warn!(media.id = id, error = %e, "user data: change not announced"),
    }
    Ok((item, data))
}
