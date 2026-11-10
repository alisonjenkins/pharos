//! `ReadPositionStore` impl for `SqliteStore` (023).
//!
//! Schema: `migrations/sqlite/0076_read_positions.sql`. One row per
//! `(user_id, item_id)`; a missing row is "never read".

use crate::sqlite::SqliteStore;
use pharos_core::{DomainError, DomainResult, MediaId, ReadPosition, ReadPositionStore, UserId};

fn map_err<E: std::fmt::Display>(e: E) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn media_id_i64(id: MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))
}

fn to_position((locator, fraction, updated_at): (String, f64, i64)) -> ReadPosition {
    ReadPosition {
        locator,
        // The column is a double; the value was an f32 on the way in.
        #[allow(clippy::cast_possible_truncation)]
        fraction: fraction as f32,
        updated_at,
    }
}

impl ReadPositionStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(user.id = %user.0, media.id = %item))]
    async fn get_read_position(
        &self,
        user: UserId,
        item: MediaId,
    ) -> DomainResult<Option<ReadPosition>> {
        let row: Option<(String, f64, i64)> = sqlx::query_as(
            "SELECT locator, fraction, updated_at FROM read_positions
             WHERE user_id = ? AND item_id = ?",
        )
        .bind(user.0.as_bytes().to_vec())
        .bind(media_id_i64(item)?)
        .fetch_optional(self.pool())
        .await
        .map_err(map_err)?;
        Ok(row.map(to_position))
    }

    #[tracing::instrument(skip(self, items), fields(user.id = %user.0, count = items.len()))]
    async fn read_positions_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> DomainResult<Vec<Option<ReadPosition>>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; items.len()].join(",");
        let sql = format!(
            "SELECT item_id, locator, fraction, updated_at FROM read_positions
             WHERE user_id = ? AND item_id IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (i64, String, f64, i64)>(sqlx::AssertSqlSafe(sql))
            .bind(user.0.as_bytes().to_vec());
        for id in items {
            q = q.bind(media_id_i64(*id)?);
        }
        let rows = q.fetch_all(self.pool()).await.map_err(map_err)?;
        let mut by_id: std::collections::HashMap<i64, ReadPosition> = rows
            .into_iter()
            .map(|(id, l, f, t)| (id, to_position((l, f, t))))
            .collect();
        items
            .iter()
            .map(|id| Ok(by_id.remove(&media_id_i64(*id)?)))
            .collect()
    }

    #[tracing::instrument(skip(self, position), fields(user.id = %user.0, media.id = %item))]
    async fn set_read_position(
        &self,
        user: UserId,
        item: MediaId,
        position: ReadPosition,
    ) -> DomainResult<bool> {
        let done = sqlx::query(
            "INSERT INTO read_positions (user_id, item_id, locator, fraction, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, item_id) DO UPDATE SET
               locator = excluded.locator,
               fraction = excluded.fraction,
               updated_at = excluded.updated_at
             WHERE excluded.updated_at >= read_positions.updated_at",
        )
        .bind(user.0.as_bytes().to_vec())
        .bind(media_id_i64(item)?)
        .bind(position.locator)
        .bind(f64::from(position.fraction))
        .bind(position.updated_at)
        .execute(self.pool())
        .await
        .map_err(map_err)?;
        Ok(done.rows_affected() > 0)
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0, limit))]
    async fn part_read_items(&self, user: UserId, limit: u32) -> DomainResult<Vec<MediaId>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT r.item_id FROM read_positions r
             LEFT JOIN user_data d ON d.user_id = r.user_id AND d.item_id = r.item_id
             WHERE r.user_id = ? AND r.fraction > 0 AND COALESCE(d.played, 0) = 0
             ORDER BY r.updated_at DESC
             LIMIT ?",
        )
        .bind(user.0.as_bytes().to_vec())
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(map_err)?;
        rows.into_iter()
            .map(|(id,)| {
                u64::try_from(id).map_err(|e| DomainError::Backend(format!("id negative: {e}")))
            })
            .collect()
    }
}
