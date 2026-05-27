//! `PreferenceStore` impl for `SqliteStore`. Migrations: 0008.

use crate::sqlite::SqliteStore;
use pharos_core::{DomainError, DomainResult, PreferenceStore, UserId};

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl PreferenceStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn get_user_configuration(&self, user: UserId) -> DomainResult<Option<String>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT config FROM user_configuration WHERE user_id = ?",
        )
        .bind(id_bytes)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(s,)| s))
    }

    #[tracing::instrument(skip(self, json), fields(user.id = %user.0.simple(), bytes = json.len()))]
    async fn set_user_configuration(&self, user: UserId, json: &str) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO user_configuration (user_id, config, updated_at)
             VALUES (?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET config = excluded.config,
                                                updated_at = excluded.updated_at",
        )
        .bind(id_bytes)
        .bind(json)
        .bind(now_unix_secs())
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple(), dp_id = %dp_id, client = %client))]
    async fn get_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
    ) -> DomainResult<Option<String>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT prefs FROM display_preferences \
             WHERE user_id = ? AND dp_id = ? AND client = ?",
        )
        .bind(id_bytes)
        .bind(dp_id)
        .bind(client)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(s,)| s))
    }

    #[tracing::instrument(skip(self, json), fields(user.id = %user.0.simple(), dp_id = %dp_id, client = %client))]
    async fn set_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
        json: &str,
    ) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO display_preferences (user_id, dp_id, client, prefs, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, dp_id, client) DO UPDATE SET prefs = excluded.prefs,
                                                               updated_at = excluded.updated_at",
        )
        .bind(id_bytes)
        .bind(dp_id)
        .bind(client)
        .bind(json)
        .bind(now_unix_secs())
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pharos_core::{SecretString, UserPolicy, UserRecord, UserStore};

    async fn seed_user(s: &SqliteStore) -> UserId {
        let uid = UserId::new();
        s.create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: SecretString::new("h"),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
        uid
    }

    #[tokio::test]
    async fn user_configuration_roundtrips() {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let uid = seed_user(&s).await;
        assert!(s.get_user_configuration(uid).await.unwrap().is_none());
        s.set_user_configuration(uid, r#"{"audio":"en"}"#).await.unwrap();
        assert_eq!(
            s.get_user_configuration(uid).await.unwrap().as_deref(),
            Some(r#"{"audio":"en"}"#)
        );
        // Upsert overwrites.
        s.set_user_configuration(uid, r#"{"audio":"de"}"#).await.unwrap();
        assert_eq!(
            s.get_user_configuration(uid).await.unwrap().as_deref(),
            Some(r#"{"audio":"de"}"#)
        );
    }

    #[tokio::test]
    async fn display_preferences_per_dpid_and_client() {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let uid = seed_user(&s).await;
        s.set_display_preferences(uid, "home", "emby", r#"{"ViewType":"x"}"#)
            .await
            .unwrap();
        s.set_display_preferences(uid, "home", "kodi", r#"{"ViewType":"y"}"#)
            .await
            .unwrap();
        assert_eq!(
            s.get_display_preferences(uid, "home", "emby")
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"ViewType":"x"}"#)
        );
        assert_eq!(
            s.get_display_preferences(uid, "home", "kodi")
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"ViewType":"y"}"#)
        );
        // Mismatched dp_id / client returns None.
        assert!(s
            .get_display_preferences(uid, "movies", "emby")
            .await
            .unwrap()
            .is_none());
    }
}
