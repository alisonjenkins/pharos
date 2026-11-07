//! Auth domain: types + IO traits. No password hashing here — that lives
//! in the `pharos-server` adapter `BuiltinAuth`. Argon2 has no business in
//! the domain layer; the trait stores opaque password hashes already
//! prepared by the auth backend.

use crate::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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

/// Jellyfin's lockout limit when a policy says `0`.
const DEFAULT_LOGIN_ATTEMPTS_BEFORE_LOCKOUT: i32 = 3;

impl UserPolicy {
    /// Whether this user may see the library with `wire_id`
    /// (`EnableAllFolders` / `EnabledFolders`).
    pub fn allows_library(&self, wire_id: &str) -> bool {
        self.enable_all_folders || self.enabled_folders.iter().any(|f| f == wire_id)
    }

    /// Whether the user may use the server on `weekday` (0 = Sunday) at
    /// `hour` (fractional, 0.0 to 24.0), both in server local time. No
    /// schedules means always (Jellyfin `IsParentalScheduleAllowed`, V578).
    pub fn within_access_schedule(&self, weekday: u8, hour: f64) -> bool {
        self.access_schedules.is_empty()
            || self
                .access_schedules
                .iter()
                .any(|s| s.allows(weekday, hour))
    }

    /// Consecutive failed password logins that disable the user, or `None`
    /// for no lockout. Jellyfin `UserManager.UpdatePolicyAsync`: `-1` is
    /// unlimited and `0` means its default of 3 (T68, V577).
    pub fn lockout_threshold(&self) -> Option<i32> {
        match self.login_attempts_before_lockout {
            n if n < 0 => None,
            0 => Some(DEFAULT_LOGIN_ATTEMPTS_BEFORE_LOCKOUT),
            n => Some(n),
        }
    }
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

impl AccessSchedule {
    /// Jellyfin `UserEntityExtensions.IsParentalScheduleAllowed`: the day
    /// matches and `start_hour <= hour <= end_hour`, both ends inclusive.
    /// `weekday` is 0 = Sunday. `Everyday`, `Weekday` (Mon-Fri) and
    /// `Weekend` (Sat, Sun) are Jellyfin's `DynamicDayOfWeek` groups; an
    /// unrecognised day matches nothing, so a typo restricts rather than
    /// opens access.
    pub fn allows(&self, weekday: u8, hour: f64) -> bool {
        let day_matches = match self.day_of_week.as_str() {
            "Sunday" => weekday == 0,
            "Monday" => weekday == 1,
            "Tuesday" => weekday == 2,
            "Wednesday" => weekday == 3,
            "Thursday" => weekday == 4,
            "Friday" => weekday == 5,
            "Saturday" => weekday == 6,
            "Everyday" => true,
            "Weekday" => (1..=5).contains(&weekday),
            "Weekend" => weekday == 0 || weekday == 6,
            _ => false,
        };
        day_matches && hour >= self.start_hour && hour <= self.end_hour
    }
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

    /// Every user whose name equals `name` ignoring case, exact match first
    /// then by name. The login lookup (Jellyfin matches names without regard
    /// to case); an empty vec means no such user. Case folding is the
    /// backend's: ASCII-only on sqlite, locale-aware `LOWER` on postgres.
    fn lookup_by_name_ignore_case(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = AuthResult<Vec<UserRecord>>> + Send;

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

    /// Atomically add one to the user's consecutive-failed-login counter and
    /// return the new value (T68, V577).
    fn record_failed_login(
        &self,
        id: UserId,
    ) -> impl std::future::Future<Output = AuthResult<i32>> + Send;

    /// Zero the consecutive-failed-login counter (successful login, or an
    /// admin re-enabling a locked-out user).
    fn reset_failed_logins(
        &self,
        id: UserId,
    ) -> impl std::future::Future<Output = AuthResult<()>> + Send;

    /// Set only `is_disabled` on the user's policy, leaving every other
    /// field as stored. The lockout uses this instead of `set_policy`, whose
    /// read-modify-write could overwrite a concurrent admin edit (V577).
    /// Idempotent.
    fn disable_user(&self, id: UserId) -> impl std::future::Future<Output = AuthResult<()>> + Send;
}

/// An API key is stored as an ordinary token whose `device_id` is this prefix
/// plus the app name. Session-level revocation must not touch these.
pub const API_KEY_DEVICE_PREFIX: &str = "apikey:";
/// Stable handle for a stored token, safe to log and keep: it is the
/// at-rest SHA-256 hash, which cannot be turned back into the secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenId(pub String);

/// A freshly issued token: the secret (shown once) and its [`TokenId`].
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub id: TokenId,
    pub secret: AuthToken,
    /// When the token stops working, unix seconds: the requested lifetime,
    /// cut short to the parent's expiry for a child token.
    pub expires_at: i64,
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
    /// Media and image GET/HEAD endpoints only: what a native client without
    /// cookie support needs to load posters and media streams via `?api_key=`.
    Media,
}

impl TokenScope {
    /// Stable string for logs and metrics labels.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Playback => "playback",
            Self::Media => "media",
        }
    }

    /// The value stored in the `scope` column. `Full` is stored as absent, so
    /// a row written by [`TokenStore::issue`] is the same row it always was.
    pub fn to_stored(self) -> Option<&'static str> {
        match self {
            Self::Full => None,
            Self::Playback | Self::Media => Some(self.label()),
        }
    }

    /// Parse the `scope` column. `NULL` (every pre-scope row) is `Full`; a
    /// value this build does not know is an error, never a silent `Full`.
    pub fn from_stored(stored: Option<&str>) -> AuthResult<Self> {
        match stored {
            None | Some("full") => Ok(Self::Full),
            Some("playback") => Ok(Self::Playback),
            Some("media") => Ok(Self::Media),
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

/// Which of a user's tokens a revoke is about to remove; the argument of
/// [`TokenStore::count_live_children`], mirroring the revoke methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSelector<'a> {
    /// The one token with this secret ([`TokenStore::revoke`]).
    Token(&'a str),
    /// Every token of this device ([`TokenStore::revoke_tokens_by_device`]).
    Device(&'a str),
    /// Every session token but this one ([`TokenStore::revoke_sessions_except`]).
    AllExcept(Option<&'a str>),
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

    /// Like [`TokenStore::issue_with_ttl`], for a token issued on the strength
    /// of `parent`: revoking `parent` by any path, or deleting its user,
    /// removes this token too, in the same statement (B543, V438). It never
    /// outlives `parent`: its expiry is the earlier of the two. Fails with
    /// [`AuthError::InvalidToken`] when `parent` is missing or expired.
    fn issue_child_with_ttl(
        &self,
        user_id: UserId,
        device_id: &str,
        ttl_secs: i64,
        scope: TokenScope,
        parent: &TokenId,
    ) -> impl std::future::Future<Output = AuthResult<IssuedToken>> + Send;

    /// How many live child tokens the tokens named by `selector` (all of
    /// `user`'s) have: what a revoke of that selection is about to take with
    /// it. Read-only and only for the revoke metric and log; the removal
    /// itself is the database cascade, which does not depend on this count.
    fn count_live_children(
        &self,
        user: UserId,
        selector: TokenSelector<'_>,
    ) -> impl std::future::Future<Output = AuthResult<u64>> + Send;

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

    /// A stable id for `token` that is not the token: what a record that
    /// must end with the token stores in its place, so the secret is never
    /// written twice.
    fn token_id(&self, token: &str) -> String;

    /// The user whose live token has [`token_id`](Self::token_id) `id`.
    /// Fails like [`resolve`](Self::resolve) once that token is revoked or
    /// expired.
    fn resolve_token_id(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = AuthResult<UserId>> + Send;

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

    /// Revoke every session token `user` holds except `keep` (all of them
    /// when `keep` is `None`). API keys (`device_id` starting with
    /// [`API_KEY_DEVICE_PREFIX`]) are not sessions and are never touched.
    /// Returns the number of rows dropped. A `keep` that is not one of the
    /// user's tokens spares nothing.
    fn revoke_sessions_except(
        &self,
        user: UserId,
        keep: Option<&str>,
    ) -> impl std::future::Future<Output = AuthResult<u64>> + Send;

    /// How many sessions `user` holds besides the one on `device_id`: the
    /// distinct devices with a live full-scope login token, API keys and
    /// restricted or child tokens excluded. What `MaxActiveSessions` caps
    /// (T68, V579).
    fn count_sessions_excluding_device(
        &self,
        user: UserId,
        device_id: &str,
    ) -> impl std::future::Future<Output = AuthResult<u32>> + Send;
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

    fn sched(day: &str, start: f64, end: f64) -> AccessSchedule {
        AccessSchedule {
            day_of_week: day.into(),
            start_hour: start,
            end_hour: end,
        }
    }

    // V578 — Jellyfin IsParentalScheduleAllowed.
    #[test]
    fn schedule_window_is_inclusive_and_day_groups_expand() {
        let s = sched("Monday", 9.0, 17.5);
        assert!(s.allows(1, 9.0) && s.allows(1, 17.5) && s.allows(1, 12.0));
        assert!(!s.allows(1, 8.99) && !s.allows(1, 17.51) && !s.allows(2, 12.0));
        let weekday = sched("Weekday", 0.0, 24.0);
        assert!((1..=5).all(|d| weekday.allows(d, 3.0)));
        assert!(!weekday.allows(0, 3.0) && !weekday.allows(6, 3.0));
        let weekend = sched("Weekend", 0.0, 24.0);
        assert!(weekend.allows(0, 3.0) && weekend.allows(6, 3.0) && !weekend.allows(3, 3.0));
        assert!((0..=6).all(|d| sched("Everyday", 0.0, 24.0).allows(d, 23.9)));
        assert!(
            !sched("Funday", 0.0, 24.0).allows(1, 1.0),
            "unknown day fails closed"
        );
    }

    #[test]
    fn no_schedules_always_allows_and_any_one_match_suffices() {
        let mut p = UserPolicy::default();
        assert!(p.within_access_schedule(3, 4.0));
        p.access_schedules = vec![sched("Monday", 9.0, 10.0), sched("Friday", 0.0, 24.0)];
        assert!(p.within_access_schedule(5, 4.0));
        assert!(p.within_access_schedule(1, 9.5));
        assert!(!p.within_access_schedule(1, 11.0));
        assert!(!p.within_access_schedule(2, 9.5));
    }

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

    #[test]
    fn token_scope_stored_roundtrip() {
        for scope in [TokenScope::Full, TokenScope::Playback, TokenScope::Media] {
            let stored = scope.to_stored();
            let parsed = TokenScope::from_stored(stored).expect("roundtrip parse");
            assert_eq!(parsed, scope);
        }
        assert_eq!(
            TokenScope::from_stored(Some("full")).expect("parse full"),
            TokenScope::Full
        );
        assert!(TokenScope::from_stored(Some("bogus")).is_err());
    }
}
