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
        sqlx::query("DELETE FROM transcode_sessions WHERE play_session_id = ?")
            .bind(play_session_id)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn prune_transcode_sessions(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        let res = sqlx::query("DELETE FROM transcode_sessions WHERE updated_at < ?")
            .bind(cutoff_unix_secs)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(res.rows_affected())
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
        let mut rows: Vec<(i64, String)> = sqlx::query_as(
            "DELETE FROM transcode_sessions WHERE play_session_id IN ( \
               SELECT play_session_id FROM transcode_sessions \
               WHERE holder_user_id = ? AND play_session_id <> ? AND updated_at >= ? \
               ORDER BY updated_at DESC, play_session_id DESC LIMIT -1 OFFSET ?) \
             RETURNING updated_at, play_session_id",
        )
        .bind(holder.0.simple().to_string())
        .bind(except_play_session_id)
        .bind(updated_since_unix_secs)
        .bind(offset)
        .fetch_all(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.sort_unstable();
        Ok(rows.into_iter().map(|(_, id)| id).collect())
    }
}
