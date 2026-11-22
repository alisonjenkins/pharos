//! `TranscodeSessionStore` impl for `SqliteStore`. Migration 0035.
//!
//! The failover breadcrumb (Phase B1): a replica that never ran the original
//! `playback_info` negotiation can load the `Decision` + source probe from
//! here (as opaque JSON) and serve the next segment instead of 410-ing.

use crate::sqlite::SqliteStore;
use pharos_core::{
    DomainError, DomainResult, MediaId, PersistedTranscodeSession, TranscodeSessionStore, UserId,
};

fn media_id_i64(id: MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))
}

type SessionRow = crate::TranscodeSessionRow;

/// B1490: remembers the grant epoch of each claimed session a removal took
/// ([`TranscodeSessionStore::transcode_session_end`]), in the removal's own
/// transaction, so the record and the removal land together or not at all.
async fn record_ends(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ended: Vec<(String, i64)>,
) -> DomainResult<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    for (play_session_id, epoch) in ended {
        if epoch <= 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO transcode_session_ends (play_session_id, grant_epoch, ended_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT (play_session_id) DO UPDATE \
             SET grant_epoch = excluded.grant_epoch, ended_at = excluded.ended_at",
        )
        .bind(&play_session_id)
        .bind(epoch)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
    }
    Ok(())
}

impl TranscodeSessionStore for SqliteStore {
    #[tracing::instrument(skip(self, play_session_id, session), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn upsert_transcode_session(
        &self,
        play_session_id: &str,
        session: &PersistedTranscodeSession,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        let media_id = media_id_i64(session.media_id)?;
        sqlx::query(
            "INSERT INTO transcode_sessions \
             (play_session_id, media_id, decision_json, source_probe_json, \
              burn_subtitle_indices_json, holder_user_id, holder_token_id, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(play_session_id) DO UPDATE SET \
               media_id = excluded.media_id, \
               decision_json = excluded.decision_json, \
               source_probe_json = excluded.source_probe_json, \
               burn_subtitle_indices_json = excluded.burn_subtitle_indices_json, \
               holder_user_id = excluded.holder_user_id, \
               holder_token_id = excluded.holder_token_id, \
               updated_at = excluded.updated_at",
        )
        .bind(play_session_id)
        .bind(media_id)
        .bind(&session.decision_json)
        .bind(&session.source_probe_json)
        .bind(&session.burn_subtitle_indices_json)
        .bind(
            session
                .holder
                .as_ref()
                .map(|h| h.user_id.0.simple().to_string()),
        )
        .bind(session.holder.as_ref().map(|h| h.token_id.as_str()))
        .bind(now_unix_secs)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, play_session_id, holder), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn update_transcode_session_holder(
        &self,
        play_session_id: &str,
        holder: &pharos_core::SessionHolder,
        now_unix_secs: i64,
        epoch: Option<i64>,
    ) -> DomainResult<bool> {
        let done = sqlx::query(
            "UPDATE transcode_sessions \
             SET holder_user_id = ?, holder_token_id = ?, updated_at = ? \
             WHERE play_session_id = ? AND (? IS NULL OR grant_epoch <= ?)",
        )
        .bind(holder.user_id.0.simple().to_string())
        .bind(holder.token_id.as_str())
        .bind(now_unix_secs)
        .bind(play_session_id)
        .bind(epoch)
        .bind(epoch)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(done.rows_affected() > 0)
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn get_transcode_session(
        &self,
        play_session_id: &str,
        updated_since_unix_secs: i64,
    ) -> DomainResult<Option<PersistedTranscodeSession>> {
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT media_id, decision_json, source_probe_json, \
                    burn_subtitle_indices_json, holder_user_id, holder_token_id \
             FROM transcode_sessions WHERE play_session_id = ? AND updated_at >= ?",
        )
        .bind(play_session_id)
        .bind(updated_since_unix_secs)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(crate::session_from_row))
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn remove_transcode_session(&self, play_session_id: &str) -> DomainResult<()> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let ended: Option<i64> = sqlx::query_scalar(
            "DELETE FROM transcode_sessions WHERE play_session_id = ? RETURNING grant_epoch",
        )
        .bind(play_session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        record_ends(
            &mut tx,
            ended
                .map(|e| (play_session_id.to_owned(), e))
                .into_iter()
                .collect(),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))
    }

    #[tracing::instrument(skip(self))]
    async fn prune_transcode_sessions(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        // Old ends go first, so the ends recorded below for the rows this
        // prune removes stay for a stop that comes after it.
        sqlx::query("DELETE FROM transcode_session_ends WHERE ended_at < ?")
            .bind(cutoff_unix_secs)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        // A claimed row the prune removes records its epoch like any other
        // removal (B1490), so a grant still holding it can tell its stop
        // from a row that was never its own.
        let removed: Vec<(String, i64)> = sqlx::query_as(
            "DELETE FROM transcode_sessions WHERE updated_at < ? \
             RETURNING play_session_id, grant_epoch",
        )
        .bind(cutoff_unix_secs)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let count = u64::try_from(removed.len())
            .map_err(|e| DomainError::Backend(format!("prune count overflow: {e}")))?;
        record_ends(&mut tx, removed).await?;
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(count)
    }

    #[tracing::instrument(skip(self, except_play_session_id))]
    async fn trim_transcode_sessions_for_holder(
        &self,
        holder: UserId,
        except_play_session_id: &str,
        keep: usize,
        updated_since_unix_secs: i64,
    ) -> DomainResult<Vec<String>> {
        let offset = i64::try_from(keep.saturating_sub(1))
            .map_err(|e| DomainError::Backend(format!("keep overflow: {e}")))?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "DELETE FROM transcode_sessions WHERE play_session_id IN ( \
               SELECT play_session_id FROM transcode_sessions \
               WHERE holder_user_id = ? AND play_session_id <> ? AND updated_at >= ? \
               ORDER BY updated_at DESC, play_session_id DESC LIMIT -1 OFFSET ?) \
             RETURNING updated_at, play_session_id, grant_epoch",
        )
        .bind(holder.0.simple().to_string())
        .bind(except_play_session_id)
        .bind(updated_since_unix_secs)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let ended = rows.iter().map(|(_, id, e)| (id.clone(), *e)).collect();
        record_ends(&mut tx, ended).await?;
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.sort_unstable();
        Ok(rows.into_iter().map(|(_, id, _)| id).collect())
    }

    #[tracing::instrument(skip(self, play_session_id, source), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn lease_transcode_session(
        &self,
        play_session_id: &str,
        source: &str,
        lease_until_unix_secs: i64,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "UPDATE transcode_sessions \
             SET grant_epoch = 1, grant_lease_until = ?, grant_source = ? \
             WHERE play_session_id = ? AND grant_epoch = 0 \
             RETURNING grant_epoch",
        )
        .bind(lease_until_unix_secs)
        .bind(source)
        .bind(play_session_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))
    }

    #[tracing::instrument(skip(self, play_session_id, source, holder), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn take_over_transcode_session(
        &self,
        play_session_id: &str,
        source: &str,
        holder: &pharos_core::SessionHolder,
        now_unix_secs: i64,
        updated_since_unix_secs: i64,
        lease_until_unix_secs: i64,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "UPDATE transcode_sessions \
             SET grant_epoch = grant_epoch + 1, grant_lease_until = ?, \
                 holder_token_id = ?, updated_at = ? \
             WHERE play_session_id = ? AND grant_epoch > 0 AND grant_source = ? \
               AND holder_user_id = ? AND updated_at >= ? \
               AND (grant_lease_until IS NULL OR grant_lease_until < ?) \
             RETURNING grant_epoch",
        )
        .bind(lease_until_unix_secs)
        .bind(holder.token_id.as_str())
        .bind(now_unix_secs)
        .bind(play_session_id)
        .bind(source)
        .bind(holder.user_id.0.simple().to_string())
        .bind(updated_since_unix_secs)
        .bind(now_unix_secs)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn renew_transcode_session_lease(
        &self,
        play_session_id: &str,
        epoch: i64,
        lease_until_unix_secs: i64,
    ) -> DomainResult<bool> {
        let done = sqlx::query(
            "UPDATE transcode_sessions SET grant_lease_until = ? \
             WHERE play_session_id = ? AND grant_epoch = ? \
               AND grant_lease_until IS NOT NULL",
        )
        .bind(lease_until_unix_secs)
        .bind(play_session_id)
        .bind(epoch)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(done.rows_affected() > 0)
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn release_transcode_session_lease(
        &self,
        play_session_id: &str,
        epoch: i64,
    ) -> DomainResult<bool> {
        let done = sqlx::query(
            "UPDATE transcode_sessions SET grant_lease_until = NULL \
             WHERE play_session_id = ? AND grant_epoch = ?",
        )
        .bind(play_session_id)
        .bind(epoch)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(done.rows_affected() > 0)
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn transcode_session_grant(
        &self,
        play_session_id: &str,
    ) -> DomainResult<Option<pharos_core::SessionGrant>> {
        let row: Option<(i64, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT grant_epoch, grant_lease_until, grant_source FROM transcode_sessions WHERE play_session_id = ?",
        )
        .bind(play_session_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(epoch, lease_until_unix_secs, source)| pharos_core::SessionGrant {
                epoch,
                lease_until_unix_secs,
                source,
            },
        ))
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn remove_transcode_session_at(
        &self,
        play_session_id: &str,
        epoch: i64,
    ) -> DomainResult<bool> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let ended: Option<i64> = sqlx::query_scalar(
            "DELETE FROM transcode_sessions WHERE play_session_id = ? AND grant_epoch <= ? \
             RETURNING grant_epoch",
        )
        .bind(play_session_id)
        .bind(epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // The end is recorded at the stopping grant's own epoch, not the
        // row's: a grant whose first lease errored stops a row still at
        // epoch 0, and a retry after a lost reply must still find its end.
        record_ends(
            &mut tx,
            ended
                .map(|_| (play_session_id.to_owned(), epoch))
                .into_iter()
                .collect(),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(ended.is_some())
    }

    #[tracing::instrument(skip(self, play_session_id), fields(psid = %pharos_core::secret_prefix(play_session_id)))]
    async fn transcode_session_end(&self, play_session_id: &str) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "SELECT grant_epoch FROM transcode_session_ends WHERE play_session_id = ?",
        )
        .bind(play_session_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))
    }
}
