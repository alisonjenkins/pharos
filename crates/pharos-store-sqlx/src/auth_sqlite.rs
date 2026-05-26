//! `UserStore` + `TokenStore` impls for `SqliteStore`.

use crate::sqlite::SqliteStore;
use pharos_core::{
    AuthError, AuthResult, AuthToken, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use uuid::Uuid;

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn map_sqlx<E: std::fmt::Display>(e: E) -> AuthError {
    AuthError::Backend(e.to_string())
}

impl UserStore for SqliteStore {
    #[tracing::instrument(skip(self, record), fields(user.name = %record.name))]
    async fn create(&self, record: UserRecord) -> AuthResult<()> {
        let id_bytes = record.id.0.as_bytes().to_vec();
        let admin: i64 = if record.policy.admin { 1 } else { 0 };
        let res = sqlx::query(
            "INSERT INTO users (id, name, password_hash, admin) VALUES (?, ?, ?, ?)",
        )
        .bind(id_bytes)
        .bind(&record.name)
        .bind(record.password_hash.expose())
        .bind(admin)
        .execute(self.pool())
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                Err(AuthError::Conflict)
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    #[tracing::instrument(skip(self), fields(user.name = %name))]
    async fn lookup_by_name(&self, name: &str) -> AuthResult<UserRecord> {
        let row: Option<(Vec<u8>, String, String, i64)> = sqlx::query_as(
            "SELECT id, name, password_hash, admin FROM users WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
        let id_bytes = id.0.as_bytes().to_vec();
        let row: Option<(Vec<u8>, String, String, i64)> = sqlx::query_as(
            "SELECT id, name, password_hash, admin FROM users WHERE id = ?",
        )
        .bind(id_bytes)
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }
}

fn record_from_row(row: (Vec<u8>, String, String, i64)) -> AuthResult<UserRecord> {
    let uuid =
        Uuid::from_slice(&row.0).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
    Ok(UserRecord {
        id: UserId(uuid),
        name: row.1,
        password_hash: SecretString::new(row.2),
        policy: UserPolicy { admin: row.3 != 0 },
    })
}

impl TokenStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(user.id = %user_id, device = %device_id))]
    async fn issue(&self, user_id: UserId, device_id: &str) -> AuthResult<AuthToken> {
        let token = Uuid::new_v4().simple().to_string();
        let user_bytes = user_id.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO auth_tokens (token, user_id, device_id, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&token)
        .bind(user_bytes)
        .bind(device_id)
        .bind(now_unix_secs())
        .execute(self.pool())
        .await
        .map_err(map_sqlx)?;
        Ok(AuthToken(SecretString::new(token)))
    }

    #[tracing::instrument(skip(self, token))]
    async fn resolve(&self, token: &str) -> AuthResult<UserId> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT user_id FROM auth_tokens WHERE token = ?")
                .bind(token)
                .fetch_optional(self.pool())
                .await
                .map_err(map_sqlx)?;
        let (bytes,) = row.ok_or(AuthError::InvalidToken)?;
        let uuid =
            Uuid::from_slice(&bytes).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
        Ok(UserId(uuid))
    }

    #[tracing::instrument(skip(self, token))]
    async fn revoke(&self, token: &str) -> AuthResult<()> {
        sqlx::query("DELETE FROM auth_tokens WHERE token = ?")
            .bind(token)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }
}
