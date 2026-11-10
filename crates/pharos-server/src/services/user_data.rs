//! A user's own state on an item: played and favourite (spec 017 US5). Both
//! wire layers write it here, so marking an item from either API checks the
//! same policy and tells every connected jellyfin-web the same thing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use pharos_core::{
    DomainError, MediaItem, MediaKind, MediaStore, ReadPosition, ReadPositionStore, User,
    UserDataStore, UserId, UserItemData,
};

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

/// A played or favourite change both wire layers can ask for on a stored
/// item, or on every episode of a synthesised series or season.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Played(bool),
    Favorite(bool),
}

impl Mark {
    /// The change on one stored item.
    pub fn apply(self, d: &mut UserItemData) {
        match self {
            Self::Played(played) => apply_played(d, played),
            Self::Favorite(favorite) => apply_favorite(d, favorite),
        }
    }
}

/// Applies `mark` to every episode of synthesised series or season `id`
/// that `user` may see (B36, T063); see [`update_folder`].
pub async fn set_folder(
    state: &AppState,
    user: &User,
    id: &str,
    mark: Mark,
) -> Result<UserItemData, DetailError> {
    update_folder(state, user, id, |d| mark.apply(d)).await
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

/// The longest locator `set_read_position` stores, in bytes (FR-011).
pub const MAX_READ_LOCATOR_BYTES: usize = 256;

/// A saved fraction at or past this marks the book read.
pub const READ_FINISHED_FRACTION: f32 = 0.99;

/// Why `set_read_position` stored nothing (FR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRefusal {
    NotBook,
    Unreadable,
    BadLocator,
    BadFraction,
}

impl ReadRefusal {
    /// The `outcome` label of `pharos_read_position_total`.
    pub fn outcome(self) -> &'static str {
        match self {
            Self::NotBook => "not_book",
            Self::Unreadable => "unreadable",
            Self::BadLocator => "bad_locator",
            Self::BadFraction => "bad_fraction",
        }
    }

    /// The argument the caller got wrong, for the wire error.
    pub fn field(self) -> &'static str {
        match self {
            Self::NotBook | Self::Unreadable => "id",
            Self::BadLocator => "locator",
            Self::BadFraction => "fraction",
        }
    }
}

/// Why `set_read_position` has no position to answer with.
#[derive(Debug)]
pub enum ReadError {
    Refused(ReadRefusal),
    Item(DetailError),
}

/// The unix second a reading happened, from the client's `readAt` in unix
/// milliseconds: 0 or less means now (old clients send nothing), and one ahead
/// of `now_secs` counts as now so a fast clock cannot outrank every later
/// reading (FR-002). Whole seconds: the `updated_at` column's unit.
pub fn read_time_secs(read_at_ms: i64, now_secs: i64) -> i64 {
    if read_at_ms <= 0 {
        return now_secs;
    }
    (read_at_ms / 1000).min(now_secs)
}

fn count_read_position(outcome: &'static str) {
    metrics::counter!("pharos_read_position_total", "outcome" => outcome).increment(1);
}

/// Saves where `user` is in book `id`: the reader's opaque `locator` and the
/// whole-book `fraction`. Stores nothing for an item that is not a readable
/// book, a locator that is empty or over [`MAX_READ_LOCATOR_BYTES`], or a
/// fraction that is not in 0 to 1 (V355). A fraction of
/// [`READ_FINISHED_FRACTION`] or more also marks the book played; the resume
/// position `last_played_position_ticks` is never written (V354). `read_at_ms`
/// is when the page was read (see [`read_time_secs`]); a reading older than
/// the stored one is not stored and does not mark the book played (V1131).
/// Answers the item, the user's play state, the position as stored and
/// whether this reading was the one stored.
pub async fn set_read_position(
    state: &AppState,
    user: &User,
    id: u64,
    locator: &str,
    fraction: f32,
    read_at_ms: i64,
) -> Result<(MediaItem, UserItemData, ReadPosition, bool), ReadError> {
    if let Err(hidden) = crate::services::access::check_item(state, user, id, "read_position").await
    {
        count_read_position("hidden");
        return Err(ReadError::Item(DetailError::Hidden(hidden)));
    }
    let item = state.stores.get(id).await.map_err(|e| {
        ReadError::Item(match e {
            DomainError::NotFound(_) => DetailError::NotFound,
            other => DetailError::Store(other),
        })
    })?;
    let refusal = match item.book.as_ref() {
        _ if item.kind != MediaKind::Book => Some(ReadRefusal::NotBook),
        Some(book) if !book.format.readable_by_client() => Some(ReadRefusal::Unreadable),
        None => Some(ReadRefusal::NotBook),
        Some(_) if locator.is_empty() || locator.len() > MAX_READ_LOCATOR_BYTES => {
            Some(ReadRefusal::BadLocator)
        }
        Some(_) if !(fraction.is_finite() && (0.0..=1.0).contains(&fraction)) => {
            Some(ReadRefusal::BadFraction)
        }
        Some(_) => None,
    };
    if let Some(refusal) = refusal {
        count_read_position(refusal.outcome());
        tracing::info!(
            media.id = id,
            user.id = %user.id.0,
            reason = refusal.outcome(),
            locator_bytes = locator.len(),
            "read position refused, nothing stored"
        );
        return Err(ReadError::Refused(refusal));
    }
    let position = ReadPosition {
        locator: locator.to_owned(),
        fraction,
        updated_at: read_time_secs(read_at_ms, crate::sync_recovery::now_unix_secs()),
    };
    let stored = state
        .stores
        .set_read_position(user.id, id, position.clone())
        .await
        .map_err(|e| ReadError::Item(DetailError::Store(e)))?;
    if !stored {
        let sent_updated_at = position.updated_at;
        let kept = state
            .stores
            .get_read_position(user.id, id)
            .await
            .map_err(|e| ReadError::Item(DetailError::Store(e)))?
            .unwrap_or(position);
        let data = state
            .stores
            .get_user_data(user.id, id)
            .await
            .map_err(|e| ReadError::Item(DetailError::Store(e)))?;
        count_read_position("stale");
        tracing::info!(
            media.id = id,
            user.id = %user.id.0,
            sent_updated_at,
            kept_updated_at = kept.updated_at,
            "read position older than the stored one, kept the stored"
        );
        return Ok((item, data, kept, false));
    }
    let finished = fraction >= READ_FINISHED_FRACTION;
    let data = if finished {
        // Played without `apply_played`, which would zero the resume ticks.
        let (_, data) = update(state, user, id, |d| {
            if !d.played {
                d.played = true;
                d.play_count = d.play_count.saturating_add(1);
                d.last_played_at = crate::sync_recovery::now_unix_secs();
            }
        })
        .await
        .map_err(ReadError::Item)?;
        data
    } else {
        state
            .stores
            .get_user_data(user.id, id)
            .await
            .map_err(|e| ReadError::Item(DetailError::Store(e)))?
    };
    count_read_position(if finished { "finished" } else { "saved" });
    Ok((item, data, position, true))
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

/// The stored items under synthesised series or season `id` (its 32-hex
/// wire id) that `user` may see.
pub async fn folder_items(
    state: &AppState,
    user: &User,
    id: &str,
) -> Result<Vec<MediaItem>, DetailError> {
    use crate::api::jellyfin::dto::{season_id_for_key, series_id_for_key};
    let all = state
        .list_items_cached()
        .await
        .map_err(DetailError::Store)?;
    let mut items: Vec<MediaItem> = all
        .iter()
        .filter(|item| {
            let Some(series) = item.series.as_ref() else {
                return false;
            };
            if series_id_for_key(series.identity_folder(), &series.series_name) == id {
                return true;
            }
            series.season_number.is_some_and(|n| {
                season_id_for_key(series.identity_folder(), &series.series_name, n) == id
            })
        })
        .cloned()
        .collect();
    crate::services::access::retain_visible(state, user, &mut items, |e| e.id, "user_data")
        .await
        .map_err(DetailError::Hidden)?;
    Ok(items)
}

/// What `user` has marked on synthesised series or season `id`, from one
/// bulk read: played once they have played every item under it they may
/// see, a favourite while every one is a favourite (marking the folder marks
/// each, B1080). Both false when they may see none.
pub async fn folder_marks(
    state: &AppState,
    user: &User,
    id: &str,
) -> Result<FolderMarks, DetailError> {
    let items = folder_items(state, user, id).await?;
    if items.is_empty() {
        return Ok(FolderMarks::default());
    }
    let ids: Vec<_> = items.iter().map(|i| i.id).collect();
    let data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(DetailError::Store)?;
    Ok(FolderMarks::of(&data))
}

/// A series or season's marks, as [`folder_marks`] answers them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderMarks {
    pub played: bool,
    pub favorite: bool,
}

impl FolderMarks {
    /// The marks of a folder holding items with `data`: each true only
    /// when every item has it, so an empty folder has neither.
    pub fn of(data: &[UserItemData]) -> Self {
        let all = |f: fn(&UserItemData) -> bool| !data.is_empty() && data.iter().all(f);
        Self {
            played: all(|d| d.played),
            favorite: all(|d| d.is_favorite),
        }
    }
}

/// Applies `change` to `user`'s state on every item under synthesised
/// series or season `id` they may see, and announces every new state plus
/// a folder entry keyed on `id`, so a series or season page updates by its
/// Key (B36). Answers the folder's state; `NotFound` when `id` names no
/// series or season with a visible item.
pub async fn update_folder(
    state: &AppState,
    user: &User,
    id: &str,
    change: impl Fn(&mut UserItemData),
) -> Result<UserItemData, DetailError> {
    let items = folder_items(state, user, id).await?;
    if items.is_empty() {
        return Err(DetailError::NotFound);
    }
    let mut entries = Vec::with_capacity(items.len() + 1);
    let mut folder = UserItemData::default();
    change(&mut folder);
    let user_key = user.id.0.simple().to_string();
    for item in items {
        let data = match write_one(state, user, &item, &change).await {
            Ok(data) => data,
            Err(e) => {
                // Announce what was written before the failure, so every
                // client shows the episodes that did change.
                if !entries.is_empty() {
                    state.notify_user_data_changed(&user_key, entries);
                }
                return Err(e);
            }
        };
        let runtime = item.probe.run_time_ticks().unwrap_or(0);
        let dto = UserItemDataDto::from_domain_with_runtime(item.id, data, runtime);
        match serde_json::to_value(&dto) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                tracing::warn!(media.id = item.id, error = %e, "user data: change not announced")
            }
        }
    }
    // Typed, so the kotlin-required UserData field set stays complete (B78/V38).
    let folder_dto =
        UserItemDataDto::folder(id, folder.played, folder.play_count, folder.is_favorite);
    match serde_json::to_value(&folder_dto) {
        Ok(entry) => entries.push(entry),
        Err(e) => tracing::warn!(folder.id = id, error = %e, "user data: change not announced"),
    }
    state.notify_user_data_changed(&user_key, entries);
    Ok(folder)
}

/// Applies `change` to `user`'s state on `item` under its write lock.
async fn write_one(
    state: &AppState,
    user: &User,
    item: &MediaItem,
    change: &impl Fn(&mut UserItemData),
) -> Result<UserItemData, DetailError> {
    let _write_guard = state.user_data_writes.lock(user.id, item.id).await;
    let mut data = state
        .stores
        .get_user_data(user.id, item.id)
        .await
        .map_err(DetailError::Store)?;
    change(&mut data);
    state
        .stores
        .set_user_data(user.id, item.id, data)
        .await
        .map_err(DetailError::Store)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-002: a read time is whole seconds, 0 or less is now, a future one is
    /// now.
    #[test]
    fn read_time_is_clamped_to_now() {
        assert_eq!(
            read_time_secs(1_700_000_123_999, 1_800_000_000),
            1_700_000_123
        );
        assert_eq!(read_time_secs(0, 1_800_000_000), 1_800_000_000);
        assert_eq!(read_time_secs(-5, 1_800_000_000), 1_800_000_000);
        assert_eq!(read_time_secs(i64::MAX, 1_800_000_000), 1_800_000_000);
    }

    /// A folder is watched, or a favourite, only when every item in it is
    /// (B1080); an empty one is neither.
    #[test]
    fn folder_marks_need_every_item() {
        let item = |played, is_favorite| UserItemData {
            played,
            is_favorite,
            ..UserItemData::default()
        };
        assert_eq!(FolderMarks::of(&[]), FolderMarks::default());
        assert_eq!(
            FolderMarks::of(&[item(true, true), item(true, true)]),
            FolderMarks {
                played: true,
                favorite: true
            }
        );
        assert_eq!(
            FolderMarks::of(&[item(true, true), item(false, false)]),
            FolderMarks::default()
        );
        assert_eq!(
            FolderMarks::of(&[item(true, false), item(true, true)]),
            FolderMarks {
                played: true,
                favorite: false
            }
        );
    }

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
