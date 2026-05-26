//! Auth domain: types + IO traits. No password hashing here — that lives
//! in the `pharos-server` adapter `BuiltinAuth`. Argon2 has no business in
//! the domain layer; the trait stores opaque password hashes already
//! prepared by the auth backend.

use crate::SecretString;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

impl UserId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for UserId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserPolicy {
    pub admin: bool,
}

/// Public user projection — no password hash. Safe to send to clients
/// (after additional auth checks at the API layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub policy: UserPolicy,
    /// Derived at projection time so the public surface knows whether
    /// a hash was set without having to expose the hash itself.
    pub has_password: bool,
}

/// Internal record carrying the password hash. Only `UserStore` callers
/// (auth backend) see this.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: UserId,
    pub name: String,
    pub password_hash: SecretString,
    pub policy: UserPolicy,
}

impl UserRecord {
    pub fn into_user(self) -> User {
        let has_password = !self.password_hash.expose().is_empty();
        User {
            id: self.id,
            name: self.name,
            policy: self.policy,
            has_password,
        }
    }
}

/// Auth tokens are opaque high-entropy strings. Wrap in `SecretString`
/// so accidental logs render `<redacted>` (V8).
#[derive(Debug, Clone)]
pub struct AuthToken(pub SecretString);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("invalid token")]
    InvalidToken,
    #[error("user not found")]
    UserNotFound,
    #[error("user name already taken")]
    Conflict,
    #[error("backend: {0}")]
    Backend(String),
}

pub type AuthResult<T> = Result<T, AuthError>;

/// Persistence of users (incl. password hashes).
pub trait UserStore: Send + Sync {
    fn create(
        &self,
        record: UserRecord,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    fn lookup_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = AuthResult<UserRecord>> + Send;

    fn get(
        &self,
        id: UserId,
    ) -> impl std::future::Future<Output = AuthResult<UserRecord>> + Send;

    /// List every user. Ordered by name for stable admin-UI rendering.
    /// T46 — admin endpoints need this for `/Users` (admin variant).
    fn list(
        &self,
    ) -> impl std::future::Future<Output = AuthResult<Vec<UserRecord>>> + Send;

    /// Drop a user + cascade their tokens + user_data rows. T46.
    fn delete(
        &self,
        id: UserId,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    /// Overwrite the user's `UserPolicy`. T46 — admin endpoint
    /// `POST /Users/{id}/Policy`.
    fn set_policy(
        &self,
        id: UserId,
        policy: UserPolicy,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;
}

/// Persistence of session tokens.
pub trait TokenStore: Send + Sync {
    fn issue(
        &self,
        user_id: UserId,
        device_id: &str,
    ) -> impl std::future::Future<Output = AuthResult<AuthToken>> + Send;

    fn resolve(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = AuthResult<UserId>> + Send;

    fn revoke(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;
}

/// High-level auth backend. Wraps a `UserStore` + password verifier.
/// `BuiltinAuth` in `pharos-server` is the canonical impl (Argon2).
pub trait AuthBackend: Send + Sync {
    fn authenticate(
        &self,
        name: &str,
        password: &SecretString,
    ) -> impl std::future::Future<Output = AuthResult<User>> + Send;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn user_id_new_is_unique() {
        let a = UserId::new();
        let b = UserId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn auth_error_display() {
        assert_eq!(AuthError::InvalidCredentials.to_string(), "invalid credentials");
        assert_eq!(AuthError::InvalidToken.to_string(), "invalid token");
    }

    #[test]
    fn user_record_strips_password_in_into_user() {
        let rec = UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: SecretString::new("$argon2id$..."),
            policy: UserPolicy { admin: true },
        };
        let user = rec.into_user();
        assert_eq!(user.name, "ali");
        assert!(user.policy.admin);
    }
}
