//! `SyncGroupStore` impl for `SqliteStore`. Migration 0036.
//!
//! Phase B4 (zero-downtime deploys): the persisted snapshot a replica
//! re-hydrates a SyncPlay group from when it takes over ownership after the
//! previous owner drained. SQLite is single-replica, so its groups never
//! actually leave the process — this impl exists for backend parity + tests.

use crate::sqlite::SqliteStore;
use pharos_core::{DomainError, DomainResult, PersistedSyncGroup, SyncGroupStore};

impl SyncGroupStore for SqliteStore {
    #[tracing::instrument(skip(self, group), fields(group_id = %group.group_id))]
    async fn upsert_sync_group(
        &self,
        group: &PersistedSyncGroup,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO sync_groups \
             (group_id, epoch_unix_ms, state_json, updated_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(group_id) DO UPDATE SET \
               epoch_unix_ms = excluded.epoch_unix_ms, \
               state_json = excluded.state_json, \
               updated_at = excluded.updated_at",
        )
        .bind(&group.group_id)
        .bind(group.epoch_unix_ms)
        .bind(&group.state_json)
        .bind(now_unix_secs)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn get_sync_group(&self, group_id: &str) -> DomainResult<Option<PersistedSyncGroup>> {
        let row: Option<(i64, String, i64)> = sqlx::query_as(
            "SELECT epoch_unix_ms, state_json, updated_at \
             FROM sync_groups WHERE group_id = ?",
        )
        .bind(group_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(epoch_unix_ms, state_json, updated_at)| PersistedSyncGroup {
                group_id: group_id.to_string(),
                epoch_unix_ms,
                state_json,
                updated_at,
            },
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn list_sync_groups(&self) -> DomainResult<Vec<PersistedSyncGroup>> {
        let rows: Vec<(String, i64, String, i64)> = sqlx::query_as(
            "SELECT group_id, epoch_unix_ms, state_json, updated_at FROM sync_groups",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(group_id, epoch_unix_ms, state_json, updated_at)| PersistedSyncGroup {
                    group_id,
                    epoch_unix_ms,
                    state_json,
                    updated_at,
                },
            )
            .collect())
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn remove_sync_group(&self, group_id: &str) -> DomainResult<()> {
        sqlx::query("DELETE FROM sync_groups WHERE group_id = ?")
            .bind(group_id)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn prune_sync_groups(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        let res = sqlx::query("DELETE FROM sync_groups WHERE updated_at < ?")
            .bind(cutoff_unix_secs)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(res.rows_affected())
    }
}
