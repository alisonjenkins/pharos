//! A user's own state on an item: played and favourite (spec 017 US5). Both
//! wire layers write it here, so marking an item from either API checks the
//! same policy and tells every connected jellyfin-web the same thing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use pharos_core::{DomainError, MediaItem, MediaStore, User, UserDataStore, UserId, UserItemData};

use crate::api::jellyfin::dto::UserItemDataDto;
use crate::services::library::DetailError;
use crate::state::AppState;

/// One lock per (user, item) being written, so a read-modify-write of one
/// user's state on one item never interleaves with another, while writes to
/// anything else run freely. An entry lives only while someone holds or waits
/// for its lock.
#[derive(Clone, Default)]
pub struct WriteLocks(Arc<Mutex<LockMap>>);

type LockMap = HashMap<(UserId, u64), Weak<tokio::sync::Mutex<()>>>;

impl WriteLocks {
    async fn lock(&self, user: UserId, id: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.retain(|_, held| held.strong_count() > 0);
            match map.get(&(user, id)).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    map.insert((user, id), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

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
    let write_guard = state.user_data_writes.lock(user.id, id).await;
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
    drop(write_guard);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes to different items do not wait on each other, and a lock
    /// nobody holds is forgotten.
    #[tokio::test]
    async fn locks_are_per_user_and_item_and_forgotten() {
        let locks = WriteLocks::default();
        let (a, b) = (UserId::new(), UserId::new());
        let held = locks.lock(a, 1).await;
        let other_item = tokio::time::timeout(std::time::Duration::from_secs(1), locks.lock(a, 2));
        let other_user = tokio::time::timeout(std::time::Duration::from_secs(1), locks.lock(b, 1));
        assert!(other_item.await.is_ok());
        assert!(other_user.await.is_ok());
        let same = tokio::time::timeout(std::time::Duration::from_millis(50), locks.lock(a, 1));
        assert!(same.await.is_err());
        drop(held);
        let _again = locks.lock(a, 3).await;
        assert_eq!(locks.len(), 1);
    }
}
