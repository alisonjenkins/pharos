//! `UserStore` + `TokenStore` impls for `SqliteStore`.

use crate::sqlite::SqliteStore;
use pharos_core::{
    AuthError, AuthResult, AuthToken, IssuedToken, ResolvedToken, SecretString, TokenId,
    TokenScope, TokenSelector, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
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

/// A `users` row as fetched by the user-store queries:
/// `(id_bytes, name, password_hash, admin, policy_json)`.
type UserRow = (Vec<u8>, String, String, i64, Option<String>);

/// Serialize the extended policy fields to the `policy_json` column. The
/// `admin` column stays the authoritative fast-path flag (indexed, read by the
/// last-admin guard), so the JSON is the store for everything else.
pub(crate) fn policy_to_json(policy: &UserPolicy) -> String {
    serde_json::to_string(policy).unwrap_or_else(|_| "{}".to_string())
}

/// Rebuild a [`UserPolicy`] from the `admin` column + optional `policy_json`.
/// A NULL / malformed blob (pre-migration rows, or a user created before the
/// field set grew) yields a permissive [`UserPolicy::default`]; the `admin`
/// column always wins so the two never disagree.
pub(crate) fn policy_from_parts(admin: i64, json: Option<&str>) -> UserPolicy {
    let mut policy = json
        .and_then(|s| serde_json::from_str::<UserPolicy>(s).ok())
        .unwrap_or_default();
    policy.admin = admin != 0;
    policy
}

impl UserStore for SqliteStore {
    #[tracing::instrument(skip(self, record), fields(user.name = %record.name))]
    async fn create(&self, record: UserRecord) -> AuthResult<()> {
        let id_bytes = record.id.0.as_bytes().to_vec();
        let admin: i64 = if record.policy.admin { 1 } else { 0 };
        let policy_json = policy_to_json(&record.policy);
        let res = sqlx::query(
            "INSERT INTO users (id, name, password_hash, admin, policy_json) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id_bytes)
        .bind(&record.name)
        .bind(record.password_hash.expose())
        .bind(admin)
        .bind(policy_json)
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
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT id, name, password_hash, admin, policy_json FROM users WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }

    #[tracing::instrument(skip(self), fields(user.name = %name))]
    async fn lookup_by_name_ignore_case(&self, name: &str) -> AuthResult<Vec<UserRecord>> {
        let rows: Vec<UserRow> = sqlx::query_as(
            "SELECT id, name, password_hash, admin, policy_json FROM users \
             WHERE LOWER(name) = LOWER(?) ORDER BY (name = ?) DESC, name",
        )
        .bind(name)
        .bind(name)
        .fetch_all(self.pool())
        .await
        .map_err(map_sqlx)?;
        rows.into_iter().map(record_from_row).collect()
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
        let id_bytes = id.0.as_bytes().to_vec();
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT id, name, password_hash, admin, policy_json FROM users WHERE id = ?",
        )
        .bind(id_bytes)
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }

    #[tracing::instrument(skip(self))]
    async fn list(&self) -> AuthResult<Vec<UserRecord>> {
        let rows: Vec<UserRow> = sqlx::query_as(
            "SELECT id, name, password_hash, admin, policy_json FROM users \
             ORDER BY name COLLATE NOCASE",
        )
        .fetch_all(self.pool())
        .await
        .map_err(map_sqlx)?;
        rows.into_iter().map(record_from_row).collect()
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn delete(&self, id: UserId) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        // Cascades drop auth_tokens + user_data via FK; user_data
        // has ON DELETE CASCADE (migration 0004), auth_tokens has it
        // on the user_id column (migration 0002).
        let res = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id_bytes)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn set_policy(&self, id: UserId, policy: UserPolicy) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        let admin: i64 = if policy.admin { 1 } else { 0 };
        let policy_json = policy_to_json(&policy);
        let res = sqlx::query("UPDATE users SET admin = ?, policy_json = ? WHERE id = ?")
            .bind(admin)
            .bind(policy_json)
            .bind(id_bytes)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    async fn set_password(
        &self,
        id: UserId,
        password_hash: pharos_core::SecretString,
    ) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        let res = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(password_hash.expose())
            .bind(id_bytes)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn record_failed_login(&self, id: UserId) -> AuthResult<i32> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE users SET failed_login_attempts = failed_login_attempts + 1 \
             WHERE id = ? RETURNING failed_login_attempts",
        )
        .bind(id.0.as_bytes().to_vec())
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        let (n,) = row.ok_or(AuthError::UserNotFound)?;
        Ok(i32::try_from(n).unwrap_or(i32::MAX))
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn reset_failed_logins(&self, id: UserId) -> AuthResult<()> {
        let res = sqlx::query("UPDATE users SET failed_login_attempts = 0 WHERE id = ?")
            .bind(id.0.as_bytes().to_vec())
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn disable_user(&self, id: UserId) -> AuthResult<()> {
        // A NULL or malformed blob reads as the default policy, so it is
        // rewritten as `{}` before the flag is set.
        let res = sqlx::query(
            "UPDATE users SET policy_json = json_set(\
             CASE WHEN json_valid(policy_json) THEN policy_json ELSE '{}' END, \
             '$.is_disabled', json('true')) WHERE id = ?",
        )
        .bind(id.0.as_bytes().to_vec())
        .execute(self.pool())
        .await
        .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }
}

fn record_from_row(row: UserRow) -> AuthResult<UserRecord> {
    let uuid =
        Uuid::from_slice(&row.0).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
    Ok(UserRecord {
        id: UserId(uuid),
        name: row.1,
        password_hash: SecretString::new(row.2),
        policy: policy_from_parts(row.3, row.4.as_deref()),
    })
}

impl SqliteStore {
    /// The user holding the live token whose hash is `hash`. Uninstrumented:
    /// `resolve` and `resolve_token_id` each count as one store call.
    async fn user_for_token_hash(&self, hash: &str) -> AuthResult<UserId> {
        // Look up by hash and reject expired tokens (expires_at NULL = never).
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT user_id FROM auth_tokens \
             WHERE token_hash = ? AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(hash)
        .bind(now_unix_secs())
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        let (bytes,) = row.ok_or(AuthError::InvalidToken)?;
        let uuid =
            Uuid::from_slice(&bytes).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
        Ok(UserId(uuid))
    }
}

impl SqliteStore {
    /// The one insert behind every issue; `parent_hash` is the token whose
    /// revoke must take this one with it (`None` for a token of its own).
    async fn insert_token(
        &self,
        user_id: UserId,
        device_id: &str,
        ttl_secs: i64,
        scope: TokenScope,
        parent_hash: Option<&str>,
    ) -> AuthResult<IssuedToken> {
        // The raw token is returned to the client but never stored — only
        // its SHA-256 hash is persisted (a DB leak must not yield usable
        // tokens). `expires_at` bounds the token's lifetime.
        let token = Uuid::new_v4().simple().to_string();
        let id = crate::auth_token::hash(&token);
        let now = now_unix_secs();
        let user_bytes = user_id.0.as_bytes().to_vec();
        let requested_expiry = now.saturating_add(ttl_secs);
        // A child is inserted from its parent's row, so a missing or expired
        // parent inserts nothing, and the child never outlives the parent.
        let row: Option<(i64,)> = match parent_hash {
            Some(parent) => {
                sqlx::query_as(
                    "INSERT INTO auth_tokens (token_hash, user_id, device_id, created_at, expires_at, scope, parent_hash) \
                 SELECT ?1, ?2, ?3, ?4, \
                        CASE WHEN p.expires_at IS NULL OR p.expires_at > ?5 THEN ?5 ELSE p.expires_at END, \
                        ?6, p.token_hash \
                 FROM auth_tokens p \
                 WHERE p.token_hash = ?7 AND p.user_id = ?2 AND (p.expires_at IS NULL OR p.expires_at > ?4) \
                 RETURNING expires_at",
                )
                .bind(&id)
                .bind(user_bytes)
                .bind(device_id)
                .bind(now)
                .bind(requested_expiry)
                .bind(scope.to_stored())
                .bind(parent)
                .fetch_optional(self.pool())
                .await
            }
            None => {
                sqlx::query_as(
                    "INSERT INTO auth_tokens (token_hash, user_id, device_id, created_at, expires_at, scope) \
                 VALUES (?, ?, ?, ?, ?, ?) RETURNING expires_at",
                )
                .bind(&id)
                .bind(user_bytes)
                .bind(device_id)
                .bind(now)
                .bind(requested_expiry)
                .bind(scope.to_stored())
                .fetch_optional(self.pool())
                .await
            }
        }
        .map_err(map_sqlx)?;
        let (expires_at,) = row.ok_or(AuthError::InvalidToken)?;
        Ok(IssuedToken {
            id: TokenId(id),
            secret: AuthToken(SecretString::new(token)),
            expires_at,
        })
    }

    /// Live (unexpired) children of the tokens `selector` names, for the
    /// revoke metric. The removal is the `parent_hash` cascade, not this.
    async fn count_children(&self, user: UserId, selector: TokenSelector<'_>) -> AuthResult<u64> {
        let user_bytes = user.0.as_bytes().to_vec();
        let now = now_unix_secs();
        let query = match selector {
            TokenSelector::Token(token) => sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM auth_tokens c JOIN auth_tokens p ON c.parent_hash = p.token_hash WHERE p.user_id = ?1 AND (c.expires_at IS NULL OR c.expires_at > ?2) AND p.token_hash = ?3",
            )
            .bind(user_bytes)
            .bind(now)
            .bind(crate::auth_token::hash(token)),
            TokenSelector::Device(device_id) => sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM auth_tokens c JOIN auth_tokens p ON c.parent_hash = p.token_hash WHERE p.user_id = ?1 AND (c.expires_at IS NULL OR c.expires_at > ?2) AND p.device_id = ?3",
            )
            .bind(user_bytes)
            .bind(now)
            .bind(device_id.to_owned()),
            TokenSelector::AllExcept(keep) => sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM auth_tokens c JOIN auth_tokens p ON c.parent_hash = p.token_hash WHERE p.user_id = ?1 AND (c.expires_at IS NULL OR c.expires_at > ?2) AND substr(p.device_id, 1, length(?3)) <> ?3 \
                 AND (?4 IS NULL OR p.token_hash <> ?4)",
            )
            .bind(user_bytes)
            .bind(now)
            .bind(pharos_core::API_KEY_DEVICE_PREFIX)
            .bind(keep.map(crate::auth_token::hash)),
        };
        let (n,) = query.fetch_one(self.pool()).await.map_err(map_sqlx)?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

impl TokenStore for SqliteStore {
    async fn issue(&self, user_id: UserId, device_id: &str) -> AuthResult<AuthToken> {
        Ok(self
            .issue_with_ttl(
                user_id,
                device_id,
                crate::auth_token::TTL_SECS,
                TokenScope::Full,
            )
            .await?
            .secret)
    }

    #[tracing::instrument(skip(self), fields(user.id = %user_id, device = %device_id))]
    async fn issue_with_ttl(
        &self,
        user_id: UserId,
        device_id: &str,
        ttl_secs: i64,
        scope: TokenScope,
    ) -> AuthResult<IssuedToken> {
        self.insert_token(user_id, device_id, ttl_secs, scope, None)
            .await
    }

    #[tracing::instrument(skip(self, parent), fields(user.id = %user_id, device = %device_id))]
    async fn issue_child_with_ttl(
        &self,
        user_id: UserId,
        device_id: &str,
        ttl_secs: i64,
        scope: TokenScope,
        parent: &TokenId,
    ) -> AuthResult<IssuedToken> {
        self.insert_token(user_id, device_id, ttl_secs, scope, Some(&parent.0))
            .await
    }

    #[tracing::instrument(skip(self, selector), fields(user.id = %user.0.simple()))]
    async fn count_live_children(
        &self,
        user: UserId,
        selector: TokenSelector<'_>,
    ) -> AuthResult<u64> {
        self.count_children(user, selector).await
    }

    #[tracing::instrument(skip(self))]
    async fn revoke_by_id(&self, id: &TokenId) -> AuthResult<()> {
        sqlx::query("DELETE FROM auth_tokens WHERE token_hash = ?")
            .bind(&id.0)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, token))]
    async fn resolve_scoped(&self, token: &str) -> AuthResult<ResolvedToken> {
        // Look up by hash and reject expired tokens (expires_at NULL = never).
        let row: Option<(Vec<u8>, Option<String>)> = sqlx::query_as(
            "SELECT user_id, scope FROM auth_tokens \
             WHERE token_hash = ? AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(crate::auth_token::hash(token))
        .bind(now_unix_secs())
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        let (bytes, scope) = row.ok_or(AuthError::InvalidToken)?;
        let uuid =
            Uuid::from_slice(&bytes).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
        Ok(ResolvedToken {
            user: UserId(uuid),
            scope: TokenScope::from_stored(scope.as_deref())?,
        })
    }

    fn token_id(&self, token: &str) -> String {
        crate::auth_token::hash(token)
    }

    #[tracing::instrument(skip(self, id))]
    async fn resolve_token_id(&self, id: &str) -> AuthResult<UserId> {
        self.user_for_token_hash(id).await
    }

    #[tracing::instrument(skip(self, token))]
    async fn device_for_token(&self, token: &str) -> AuthResult<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT device_id FROM auth_tokens \
             WHERE token_hash = ? AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(crate::auth_token::hash(token))
        .bind(now_unix_secs())
        .fetch_optional(self.pool())
        .await
        .map_err(map_sqlx)?;
        Ok(row.map(|(device_id,)| device_id))
    }

    #[tracing::instrument(skip(self, token))]
    async fn revoke(&self, token: &str) -> AuthResult<()> {
        sqlx::query("DELETE FROM auth_tokens WHERE token_hash = ?")
            .bind(crate::auth_token::hash(token))
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn tokens_for(&self, user: UserId) -> AuthResult<Vec<pharos_core::TokenRecord>> {
        let user_bytes = user.0.as_bytes().to_vec();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT device_id, created_at FROM auth_tokens WHERE user_id = ?
             ORDER BY created_at DESC",
        )
        .bind(user_bytes)
        .fetch_all(self.pool())
        .await
        .map_err(map_sqlx)?;
        Ok(rows
            .into_iter()
            .map(
                |(device_id, issued_at_unix_secs)| pharos_core::TokenRecord {
                    device_id,
                    issued_at_unix_secs,
                },
            )
            .collect())
    }

    /// T58 phase 3 — revoke every token belonging to `user` whose
    /// `device_id` matches the supplied value. Returns the number of rows
    /// dropped so the handler can 404 on a stale id.
    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn revoke_tokens_by_device(&self, user: UserId, device_id: &str) -> AuthResult<u64> {
        let user_bytes = user.0.as_bytes().to_vec();
        let res = sqlx::query("DELETE FROM auth_tokens WHERE user_id = ? AND device_id = ?")
            .bind(user_bytes)
            .bind(device_id)
            .execute(self.pool())
            .await
            .map_err(map_sqlx)?;
        Ok(res.rows_affected())
    }

    /// Revoke all of `user`'s session tokens but `keep` (a password change
    /// keeps the caller's own session). API keys are left alone.
    #[tracing::instrument(skip(self, keep), fields(user.id = %user.0.simple()))]
    async fn revoke_sessions_except(&self, user: UserId, keep: Option<&str>) -> AuthResult<u64> {
        let user_bytes = user.0.as_bytes().to_vec();
        // An exact, case-sensitive prefix match: SQLite's LIKE ignores ASCII
        // case and Postgres's does not, so LIKE would split the backends.
        let api_key_prefix = pharos_core::API_KEY_DEVICE_PREFIX;
        let res = match keep {
            Some(token) => {
                sqlx::query(
                    "DELETE FROM auth_tokens WHERE user_id = ?1 \
                     AND substr(device_id, 1, length(?2)) <> ?2 AND token_hash <> ?3",
                )
                .bind(user_bytes)
                .bind(api_key_prefix)
                .bind(crate::auth_token::hash(token))
                .execute(self.pool())
                .await
            }
            None => {
                sqlx::query(
                    "DELETE FROM auth_tokens WHERE user_id = ?1 \
                     AND substr(device_id, 1, length(?2)) <> ?2",
                )
                .bind(user_bytes)
                .bind(api_key_prefix)
                .execute(self.pool())
                .await
            }
        }
        .map_err(map_sqlx)?;
        Ok(res.rows_affected())
    }
}
