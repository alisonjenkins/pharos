//! `UserDataStore` impl for `SqliteStore` (T33).
//!
//! Schema: `migrations/sqlite/0004_user_data.sql`. One row per
//! `(user_id, item_id)`. Missing rows are reported as `UserItemData::default()`
//! so callers don't branch on existence.

use crate::sqlite::SqliteStore;
use pharos_core::{DomainError, DomainResult, MediaId, UserDataStore, UserId, UserItemData};

fn map_err<E: std::fmt::Display>(e: E) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn media_id_i64(id: MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))
}

impl UserDataStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(user.id = %user.0, media.id = %item))]
    async fn get_user_data(
        &self,
        user: UserId,
        item: MediaId,
    ) -> DomainResult<UserItemData> {
        let id_bytes = user.0.as_bytes().to_vec();
        let item_i64 = media_id_i64(item)?;
        let row: Option<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT played, play_count, last_played_position_ticks, is_favorite, last_played_at
             FROM user_data WHERE user_id = ? AND item_id = ?",
        )
        .bind(id_bytes)
        .bind(item_i64)
        .fetch_optional(self.pool())
        .await
        .map_err(map_err)?;
        Ok(row.map(row_to_data).unwrap_or_default())
    }

    #[tracing::instrument(skip(self, data), fields(user.id = %user.0, media.id = %item))]
    async fn set_user_data(
        &self,
        user: UserId,
        item: MediaId,
        data: UserItemData,
    ) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        let item_i64 = media_id_i64(item)?;
        let played: i64 = if data.played { 1 } else { 0 };
        let fav: i64 = if data.is_favorite { 1 } else { 0 };
        let pos_i64 = i64::try_from(data.last_played_position_ticks)
            .map_err(|e| DomainError::Backend(format!("position overflow: {e}")))?;
        let pc_i64 = i64::from(data.play_count);
        sqlx::query(
            "INSERT INTO user_data
               (user_id, item_id, played, play_count, last_played_position_ticks,
                is_favorite, last_played_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id, item_id) DO UPDATE SET
               played = excluded.played,
               play_count = excluded.play_count,
               last_played_position_ticks = excluded.last_played_position_ticks,
               is_favorite = excluded.is_favorite,
               last_played_at = excluded.last_played_at",
        )
        .bind(id_bytes)
        .bind(item_i64)
        .bind(played)
        .bind(pc_i64)
        .bind(pos_i64)
        .bind(fav)
        .bind(data.last_played_at)
        .execute(self.pool())
        .await
        .map_err(map_err)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, items), fields(user.id = %user.0, count = items.len()))]
    async fn user_data_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> DomainResult<Vec<UserItemData>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let id_bytes = user.0.as_bytes().to_vec();
        // sqlx's macro `query!` can't bind a variadic IN — build the
        // placeholder list dynamically. Inputs are u64s, no SQL
        // injection risk.
        let placeholders = std::iter::repeat("?")
            .take(items.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT item_id, played, play_count, last_played_position_ticks,
                    is_favorite, last_played_at
             FROM user_data
             WHERE user_id = ? AND item_id IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(&sql);
        q = q.bind(id_bytes);
        for id in items {
            q = q.bind(media_id_i64(*id)?);
        }
        let rows = q.fetch_all(self.pool()).await.map_err(map_err)?;
        let mut by_id: std::collections::HashMap<i64, UserItemData> =
            std::collections::HashMap::with_capacity(rows.len());
        for (id, played, pc, pos, fav, lp) in rows {
            by_id.insert(id, row_to_data((played, pc, pos, fav, lp)));
        }
        let mut out = Vec::with_capacity(items.len());
        for id in items {
            let key = media_id_i64(*id)?;
            out.push(by_id.get(&key).copied().unwrap_or_default());
        }
        Ok(out)
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0))]
    async fn resumable_items(&self, user: UserId) -> DomainResult<Vec<MediaId>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT item_id FROM user_data
             WHERE user_id = ? AND last_played_position_ticks > 0 AND played = 0
             ORDER BY last_played_at DESC",
        )
        .bind(id_bytes)
        .fetch_all(self.pool())
        .await
        .map_err(map_err)?;
        rows.into_iter()
            .map(|(id,)| {
                u64::try_from(id)
                    .map_err(|e| DomainError::Backend(format!("id negative: {e}")))
            })
            .collect()
    }
}

fn row_to_data(row: (i64, i64, i64, i64, i64)) -> UserItemData {
    let (played, pc, pos, fav, lp) = row;
    UserItemData {
        played: played != 0,
        play_count: u32::try_from(pc).unwrap_or(0),
        last_played_position_ticks: u64::try_from(pos).unwrap_or(0),
        is_favorite: fav != 0,
        last_played_at: lp,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, UserPolicy, UserRecord, UserStore,
    };

    async fn fixture() -> (SqliteStore, UserId, MediaId) {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let uid = UserId::new();
        s.create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: SecretString::new("h"),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
        s.put(MediaItem {
            id: 7,
            path: "/m/x".into(),
            title: "x".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
        (s, uid, 7)
    }

    #[tokio::test]
    async fn missing_row_reads_as_default() {
        let (s, uid, id) = fixture().await;
        let d = s.get_user_data(uid, id).await.unwrap();
        assert_eq!(d, UserItemData::default());
    }

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let (s, uid, id) = fixture().await;
        let data = UserItemData {
            played: true,
            play_count: 3,
            last_played_position_ticks: 12_345_000,
            is_favorite: true,
            last_played_at: 1_700_000_000,
        };
        s.set_user_data(uid, id, data).await.unwrap();
        let back = s.get_user_data(uid, id).await.unwrap();
        assert_eq!(back, data);
    }

    #[tokio::test]
    async fn second_set_overwrites_not_duplicates() {
        let (s, uid, id) = fixture().await;
        let a = UserItemData {
            played: false,
            play_count: 1,
            last_played_position_ticks: 100,
            is_favorite: false,
            last_played_at: 1,
        };
        s.set_user_data(uid, id, a).await.unwrap();
        let b = UserItemData {
            played: true,
            play_count: 2,
            last_played_position_ticks: 0,
            is_favorite: false,
            last_played_at: 2,
        };
        s.set_user_data(uid, id, b).await.unwrap();
        let back = s.get_user_data(uid, id).await.unwrap();
        assert_eq!(back, b);
    }

    #[tokio::test]
    async fn bulk_fetch_returns_defaults_for_missing() {
        let (s, uid, _) = fixture().await;
        // Add a second item to test mix of present + missing rows.
        s.put(MediaItem {
            id: 8,
            path: "/m/y".into(),
            title: "y".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
        s.set_user_data(
            uid,
            7,
            UserItemData {
                played: true,
                play_count: 1,
                last_played_position_ticks: 0,
                is_favorite: false,
                last_played_at: 0,
            },
        )
        .await
        .unwrap();
        let v = s.user_data_bulk(uid, &[7, 8, 9999]).await.unwrap();
        assert_eq!(v.len(), 3);
        assert!(v[0].played);
        assert_eq!(v[1], UserItemData::default());
        assert_eq!(v[2], UserItemData::default());
    }

    #[tokio::test]
    async fn resumable_items_filters_played_and_zero_position() {
        let (s, uid, _) = fixture().await;
        // 3 items: id=7 resumable, id=8 played (filtered out),
        // id=9 untouched.
        for id in [8u64, 9u64] {
            s.put(MediaItem {
                id,
                path: format!("/m/{id}").into(),
                title: format!("t-{id}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
        }
        s.set_user_data(
            uid,
            7,
            UserItemData {
                played: false,
                play_count: 0,
                last_played_position_ticks: 100,
                is_favorite: false,
                last_played_at: 10,
            },
        )
        .await
        .unwrap();
        s.set_user_data(
            uid,
            8,
            UserItemData {
                played: true,
                play_count: 1,
                last_played_position_ticks: 200,
                is_favorite: false,
                last_played_at: 20,
            },
        )
        .await
        .unwrap();
        let v = s.resumable_items(uid).await.unwrap();
        assert_eq!(v, vec![7]);
    }
}
