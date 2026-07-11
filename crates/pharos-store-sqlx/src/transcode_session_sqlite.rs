//! `TranscodeSessionStore` impl for `SqliteStore`. Migration 0035.
//!
//! The failover breadcrumb (Phase B1): a replica that never ran the original
//! `playback_info` negotiation can load the `Decision` + source probe from
//! here (as opaque JSON) and serve the next segment instead of 410-ing.

use crate::sqlite::SqliteStore;
use pharos_core::{
    DomainError, DomainResult, MediaId, PersistedTranscodeSession, TranscodeSessionStore,
};

fn media_id_i64(id: MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))
}

impl TranscodeSessionStore for SqliteStore {
    #[tracing::instrument(skip(self, session), fields(psid = %play_session_id))]
    async fn upsert_transcode_session(
        &self,
        play_session_id: &str,
        session: &PersistedTranscodeSession,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        let media_id = media_id_i64(session.media_id)?;
        sqlx::query(
            "INSERT INTO transcode_sessions \
             (play_session_id, media_id, decision_json, source_probe_json, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(play_session_id) DO UPDATE SET \
               media_id = excluded.media_id, \
               decision_json = excluded.decision_json, \
               source_probe_json = excluded.source_probe_json, \
               updated_at = excluded.updated_at",
        )
        .bind(play_session_id)
        .bind(media_id)
        .bind(&session.decision_json)
        .bind(&session.source_probe_json)
        .bind(now_unix_secs)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(psid = %play_session_id))]
    async fn get_transcode_session(
        &self,
        play_session_id: &str,
    ) -> DomainResult<Option<PersistedTranscodeSession>> {
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT media_id, decision_json, source_probe_json \
             FROM transcode_sessions WHERE play_session_id = ?",
        )
        .bind(play_session_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(media_id, decision_json, source_probe_json)| PersistedTranscodeSession {
                media_id: media_id as u64,
                decision_json,
                source_probe_json,
            },
        ))
    }

    #[tracing::instrument(skip(self), fields(psid = %play_session_id))]
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
}
