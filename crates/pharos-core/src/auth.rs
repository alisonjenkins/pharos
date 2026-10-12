//! Auth domain: types + IO traits. No password hashing here — that lives
//! in the `pharos-server` adapter `BuiltinAuth`. Argon2 has no business in
//! the domain layer; the trait stores opaque password hashes already
//! prepared by the auth backend.

use crate::SecretString;
use serde::{Deserialize, Serialize};
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

/// A user's full permission set (T68). Mirrors the load-bearing fields of
/// Jellyfin's C# `UserPolicy`. Persisted as a JSON blob (`users.policy_json`)
/// alongside the fast-path `admin` column; the DTO layer maps it to the wire
/// `UserPolicyDto`.
///
/// Not `Copy` (carries `Vec`s / `String`), and not `Eq` (`AccessSchedule`
/// hours are `f64`). [`Default`] is hand-written to be fully permissive so a
/// freshly-created user is unrestricted — matching the values pharos hardcoded
/// before the field set grew (so default users' wire output is unchanged).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPolicy {
    pub admin: bool,
    /// A disabled user cannot authenticate (enforced in the auth backend).
    pub is_disabled: bool,
    /// Hidden users are omitted from the login user-picker.
    pub is_hidden: bool,
    /// When `true` the user sees every library; when `false` only the
    /// libraries whose wire id is in [`Self::enabled_folders`].
    pub enable_all_folders: bool,
    /// Library wire ids the user may browse when `!enable_all_folders`.
    pub enabled_folders: Vec<String>,
    /// Max allowed parental-rating score (see the config rating table). `None`
    /// = unrestricted. An item whose rating scores above this is filtered.
    pub max_parental_rating: Option<i32>,
    /// Item types whose unrated members are blocked (e.g. `["Movie"]`).
    pub block_unrated_items: Vec<String>,
    /// Items carrying any of these tags are hidden from the user.
    pub blocked_tags: Vec<String>,
    /// When non-empty, only items carrying one of these tags are shown.
    pub allowed_tags: Vec<String>,
    /// Time windows during which the user may access the server.
    pub access_schedules: Vec<AccessSchedule>,
    /// `0` = unlimited concurrent sessions.
    pub max_active_sessions: i32,
    /// `-1` = unlimited failed attempts before lockout.
    pub login_attempts_before_lockout: i32,
    /// `0` = no remote bitrate cap.
    pub remote_client_bitrate_limit: i32,
    pub enable_live_tv_access: bool,
    pub enable_content_downloading: bool,
    /// Jellyfin `SyncPlayUserAccessType`: `CreateAndJoinGroups` | `JoinGroups`
    /// | `None`.
    pub sync_play_access: String,
}

impl Default for UserPolicy {
    fn default() -> Self {
        Self {
            admin: false,
            is_disabled: false,
            is_hidden: false,
            enable_all_folders: true,
            enabled_folders: Vec::new(),
            max_parental_rating: None,
            block_unrated_items: Vec::new(),
            blocked_tags: Vec::new(),
            allowed_tags: Vec::new(),
            access_schedules: Vec::new(),
            max_active_sessions: 0,
            login_attempts_before_lockout: -1,
            remote_client_bitrate_limit: 0,
            enable_live_tv_access: true,
            enable_content_downloading: true,
            sync_play_access: "CreateAndJoinGroups".to_string(),
        }
    }
}

/// A recurring weekly access window (Jellyfin `AccessSchedule`). Hours are
/// fractional (`8.5` = 08:30) to match jellyfin-web's serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessSchedule {
    pub day_of_week: String,
    pub start_hour: f64,
    pub end_hour: f64,
}

impl Default for AccessSchedule {
    fn default() -> Self {
        Self {
            day_of_week: "Everyday".to_string(),
            start_hour: 0.0,
            end_hour: 24.0,
        }
    }
}

/// Public user projection — no password hash. Safe to send to clients
/// (after additional auth checks at the API layer).
#[derive(Debug, Clone, PartialEq)]
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

    fn get(&self, id: UserId) -> impl std::future::Future<Output = AuthResult<UserRecord>> + Send;

    /// List every user. Ordered by name for stable admin-UI rendering.
    /// T46 — admin endpoints need this for `/Users` (admin variant).
    fn list(&self) -> impl std::future::Future<Output = AuthResult<Vec<UserRecord>>> + Send;

    /// Drop a user + cascade their tokens + user_data rows. T46.
    fn delete(&self, id: UserId) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    /// Overwrite the user's `UserPolicy`. T46 — admin endpoint
    /// `POST /Users/{id}/Policy`.
    fn set_policy(
        &self,
        id: UserId,
        policy: UserPolicy,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    /// Atomically replace a user's password hash (single UPDATE). The
    /// password-change endpoint MUST use this rather than delete+create —
    /// a failed re-create after a delete would irreversibly destroy the
    /// account and all cascaded data.
    fn set_password(
        &self,
        id: UserId,
        password_hash: SecretString,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;
}

/// Stable handle for a stored token, safe to log and keep: it is the
/// at-rest SHA-256 hash, which cannot be turned back into the secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenId(pub String);

/// A freshly issued token: the secret (shown once) and its [`TokenId`].
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub id: TokenId,
    pub secret: AuthToken,
}

/// What a stored token may do. A closed set: a new scope is a deliberate
/// code change, and every consumer matches it exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TokenScope {
    /// The owner's full permissions. Every token issued before scopes existed,
    /// and every token a login issues.
    #[default]
    Full,
    /// Playback endpoints only: what a cast receiver needs to fetch and
    /// report on the media it was told to play.
    Playback,
}

impl TokenScope {
    /// Stable string for logs and metrics labels.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Playback => "playback",
        }
    }

    /// The value stored in the `scope` column. `Full` is stored as absent, so
    /// a row written by [`TokenStore::issue`] is the same row it always was.
    pub fn to_stored(self) -> Option<&'static str> {
        match self {
            Self::Full => None,
            Self::Playback => Some(self.label()),
        }
    }

    /// Parse the `scope` column. `NULL` (every pre-scope row) is `Full`; a
    /// value this build does not know is an error, never a silent `Full`.
    pub fn from_stored(stored: Option<&str>) -> AuthResult<Self> {
        match stored {
            None | Some("full") => Ok(Self::Full),
            Some("playback") => Ok(Self::Playback),
            Some(other) => Err(AuthError::Backend(format!(
                "unknown token scope {other:?} in auth_tokens.scope"
            ))),
        }
    }
}

/// The user and scope a presented token resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedToken {
    pub user: UserId,
    pub scope: TokenScope,
}

/// Persistence of session tokens.
pub trait TokenStore: Send + Sync {
    /// Like [`TokenStore::issue`] but with a caller-chosen lifetime in
    /// seconds and scope, and the token's id returned beside the secret.
    fn issue_with_ttl(
        &self,
        user_id: UserId,
        device_id: &str,
        ttl_secs: i64,
        scope: TokenScope,
    ) -> impl std::future::Future<Output = AuthResult<IssuedToken>> + Send;

    /// Revoke by [`TokenId`]. Idempotent: an unknown id is `Ok`.
    fn revoke_by_id(
        &self,
        id: &TokenId,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    fn issue(
        &self,
        user_id: UserId,
        device_id: &str,
    ) -> impl std::future::Future<Output = AuthResult<AuthToken>> + Send;

    /// The user and scope behind `token`.
    fn resolve_scoped(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = AuthResult<ResolvedToken>> + Send;

    /// The user behind `token`, ignoring its scope. Use [`Self::resolve_scoped`]
    /// wherever the scope decides whether a request may proceed.
    fn resolve(&self, token: &str) -> impl std::future::Future<Output = AuthResult<UserId>> + Send {
        async move { self.resolve_scoped(token).await.map(|r| r.user) }
    }

    fn revoke(&self, token: &str) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    /// The `device_id` a live token was issued to, or `None` for an unknown or
    /// expired token. Lets a request that cannot carry a device id itself (a
    /// browser WebSocket handshake) still be attributed to its device.
    fn device_for_token(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = AuthResult<Option<String>>> + Send;

    /// Tokens issued to `user`. Drives admin "active devices" list. Each
    /// entry carries the device_id (Emby-Authorization header) + issued_at
    /// unix-seconds. Token strings deliberately not exposed.
    fn tokens_for(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = AuthResult<Vec<TokenRecord>>> + Send;

    /// Revoke every token belonging to `user` whose `device_id` matches
    /// the supplied value. Returns the number of rows dropped so the
    /// caller can 404 on a stale id.
    fn revoke_tokens_by_device(
        &self,
        user: UserId,
        device_id: &str,
    ) -> impl std::future::Future<Output = AuthResult<u64>> + Send;
}

/// Per-token metadata exposed to admin endpoints. The actual token
/// string never leaves the auth backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub device_id: String,
    pub issued_at_unix_secs: i64,
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
        assert_eq!(
            AuthError::InvalidCredentials.to_string(),
            "invalid credentials"
        );
        assert_eq!(AuthError::InvalidToken.to_string(), "invalid token");
    }

    #[test]
    fn user_record_strips_password_in_into_user() {
        let rec = UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: SecretString::new("$argon2id$..."),
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        };
        let user = rec.into_user();
        assert_eq!(user.name, "ali");
        assert!(user.policy.admin);
    }
}
