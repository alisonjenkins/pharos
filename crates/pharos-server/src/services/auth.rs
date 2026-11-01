//! Token to [`User`]: the decision every authenticated call on either API
//! makes, implemented once (V172).

use pharos_core::{AuthError, TokenStore, User, UserId, UserStore};
use tokio::sync::broadcast;

use crate::state::{AppState, Stores};

/// Why a token did not yield a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRefusal {
    /// The store does not know the token.
    InvalidToken,
    /// The token resolved, but its user no longer exists.
    UserRevoked,
    /// The store could not answer (pool timeout, database down). Transient and
    /// ours, not the caller's: they keep their session and retry (B216).
    Unavailable(String),
}

impl TokenRefusal {
    /// The `reason` label of `pharos_auth_rejected_total`.
    pub fn label(&self) -> &'static str {
        match self {
            TokenRefusal::InvalidToken => "invalid_token",
            TokenRefusal::UserRevoked => "user_revoked",
            TokenRefusal::Unavailable(_) => "store_unavailable",
        }
    }
}

/// Resolves `token` to its user: one store call for an unknown token, two
/// for a known one.
pub async fn user_for_token(stores: &Stores, token: &str) -> Result<User, TokenRefusal> {
    let uid = stores
        .resolve(token)
        .await
        .map_err(|e| classify(e, TokenRefusal::InvalidToken))?;
    let record = stores
        .get(uid)
        .await
        .map_err(|e| classify(e, TokenRefusal::UserRevoked))?;
    // A disabled user's tokens stop working, not only their password.
    if record.policy.is_disabled {
        return Err(TokenRefusal::UserRevoked);
    }
    Ok(record.into_user())
}

/// A backend failure is `Unavailable` whatever stage it hit; any other error
/// at that stage is `otherwise`.
fn classify(e: AuthError, otherwise: TokenRefusal) -> TokenRefusal {
    match e {
        AuthError::Backend(detail) => TokenRefusal::Unavailable(detail),
        _ => otherwise,
    }
}

/// Why a user's tokens may have stopped resolving. The label is the `cause`
/// of `pharos_auth_revocation_announced_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeCause {
    /// The caller signed out, revoking its own token.
    Logout,
    /// An admin deleted a device, revoking its tokens.
    DeviceDeleted,
    /// An admin revoked an API key.
    ApiKeyRevoked,
    /// The user was deleted.
    UserDeleted,
    /// The user's policy changed (it may now be disabled).
    PolicyChanged,
    /// The user's password changed, so tokens issued under the old one go.
    PasswordChanged,
}

impl RevokeCause {
    pub fn label(self) -> &'static str {
        match self {
            RevokeCause::Logout => "logout",
            RevokeCause::DeviceDeleted => "device_deleted",
            RevokeCause::ApiKeyRevoked => "api_key_revoked",
            RevokeCause::UserDeleted => "user_deleted",
            RevokeCause::PolicyChanged => "policy_changed",
            RevokeCause::PasswordChanged => "password_changed",
        }
    }
}

/// Announced events a channel holds before a slow subscriber lags. A lagged
/// connection re-checks all its tokens, so overflow costs a lookup, never a
/// missed revocation.
const ANNOUNCE_CAPACITY: usize = 256;

/// Tells open native connections (T025) that some of a user's tokens may no
/// longer resolve. The message is a trigger, not a verdict: each of that
/// user's connections re-checks its own tokens against the store, so revoking
/// one device's token leaves the user's other devices connected. In-process
/// only: with more than one pharos replica, connections on the others still
/// wait for their timer (`revalidate_secs`).
#[derive(Debug, Clone)]
pub struct Revocations {
    tx: broadcast::Sender<UserId>,
}

impl Default for Revocations {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(ANNOUNCE_CAPACITY).0,
        }
    }
}

impl Revocations {
    /// Counts and logs the announcement, then sends it to every subscriber.
    pub fn announce(&self, user: UserId, cause: RevokeCause) {
        metrics::counter!("pharos_auth_revocation_announced_total", "cause" => cause.label())
            .increment(1);
        // No subscribers is the normal case with the native API off.
        // Every open native connection subscribes, whoever its user.
        let subscribers = self.tx.send(user).unwrap_or(0);
        tracing::info!(
            user_id = %user.0,
            cause = cause.label(),
            subscribers,
            "auth: revocation announced"
        );
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UserId> {
        self.tx.subscribe()
    }
}

/// Revokes `user`'s `token` (idempotent) and announces the user.
pub async fn revoke_token(
    state: &AppState,
    user: UserId,
    token: &str,
    cause: RevokeCause,
) -> Result<(), AuthError> {
    state.stores.revoke(token).await?;
    state.revocations.announce(user, cause);
    Ok(())
}

/// Revokes every token `user` holds for `device_id`; announces the user when
/// any went. Returns how many.
pub async fn revoke_device(
    state: &AppState,
    user: UserId,
    device_id: &str,
    cause: RevokeCause,
) -> Result<u64, AuthError> {
    let dropped = state
        .stores
        .revoke_tokens_by_device(user, device_id)
        .await?;
    if dropped > 0 {
        state.revocations.announce(user, cause);
    }
    Ok(dropped)
}

/// How a native sign-in arrived. The label is the `transport` of
/// `pharos_native_login_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTransport {
    /// `POST /pharos/session`, answered with the `pharos_session` cookie.
    Cookie,
    /// `Bootstrap.login` / `quickConnectPoll` over `/pharos/rpc`.
    Rpc,
}

impl LoginTransport {
    pub fn label(self) -> &'static str {
        match self {
            LoginTransport::Cookie => "cookie",
            LoginTransport::Rpc => "rpc",
        }
    }
}

/// How a native sign-in ended. The label is the `outcome` of
/// `pharos_native_login_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    /// Wrong password, unknown or disabled user: one answer for all three.
    Invalid,
    /// Refused by the login throttle before any hash (V174).
    Throttled,
    /// The user or token store failed.
    Unavailable,
    /// A browser request from an origin not in `allowed_origins`, or none.
    OriginRefused,
    /// The password was right but the user holds the most connections they
    /// may (FR-010); no token was issued.
    Capped,
    /// A Quick Connect poll for a code nobody has authorized yet. Not
    /// counted: clients poll every few seconds.
    Pending,
}

impl LoginOutcome {
    pub fn label(self) -> &'static str {
        match self {
            LoginOutcome::Ok => "ok",
            LoginOutcome::Invalid => "invalid",
            LoginOutcome::Throttled => "throttled",
            LoginOutcome::Unavailable => "unavailable",
            LoginOutcome::OriginRefused => "origin_refused",
            LoginOutcome::Capped => "capped",
            LoginOutcome::Pending => "pending",
        }
    }
}

/// Counts one native sign-in attempt by transport, method and outcome.
pub fn record_login(transport: LoginTransport, method: &'static str, outcome: LoginOutcome) {
    metrics::counter!(
        "pharos_native_login_total",
        "transport" => transport.label(),
        "method" => method,
        "outcome" => outcome.label()
    )
    .increment(1);
}

/// Why [`password_login`] issued no token.
#[derive(Debug)]
pub enum PasswordLoginError {
    Refused(crate::auth::LoginRefusal),
    /// The password was right but the token could not be stored.
    Issue(AuthError),
    /// The password was right but `admit` refused the user: they hold
    /// `held` of at most `max` connections.
    Capped {
        held: u32,
        max: u32,
    },
}

impl PasswordLoginError {
    pub fn outcome(&self) -> LoginOutcome {
        use crate::auth::LoginRefusal;
        match self {
            PasswordLoginError::Refused(LoginRefusal::Throttled(_)) => LoginOutcome::Throttled,
            PasswordLoginError::Refused(LoginRefusal::Invalid) => LoginOutcome::Invalid,
            PasswordLoginError::Refused(LoginRefusal::Backend(_))
            | PasswordLoginError::Issue(_) => LoginOutcome::Unavailable,
            PasswordLoginError::Capped { .. } => LoginOutcome::Capped,
        }
    }
}

/// A native password sign-in: the shared throttle checks the password
/// (V174), `admit` may still refuse the user (`Err((held, max))`, a
/// connection cap), then a device token is issued for `device_id` in the one
/// token store (ADR-0011). Counted and logged with its outcome either way;
/// a refused login issues no token.
pub async fn password_login(
    state: &AppState,
    transport: LoginTransport,
    name: &str,
    password: &pharos_core::SecretString,
    client_ip: std::net::IpAddr,
    device_id: &str,
    admit: impl FnOnce(&User) -> Result<(), (u32, u32)>,
) -> Result<(User, pharos_core::AuthToken), PasswordLoginError> {
    let result: Result<_, PasswordLoginError> = async {
        let user = state
            .login_throttle
            .verify(&state.auth, name, password, client_ip)
            .await
            .map_err(PasswordLoginError::Refused)?;
        admit(&user).map_err(|(held, max)| PasswordLoginError::Capped { held, max })?;
        let token = state
            .stores
            .issue(user.id, device_id)
            .await
            .map_err(PasswordLoginError::Issue)?;
        Ok((user, token))
    }
    .await;
    let outcome = match &result {
        Ok(_) => LoginOutcome::Ok,
        Err(e) => e.outcome(),
    };
    record_login(transport, "password", outcome);
    match &result {
        Ok((user, _)) => {
            tracing::info!(
                transport = transport.label(),
                user = %user.name,
                client_ip = %client_ip,
                device_id,
                "native login"
            );
            state.record_activity(
                &format!("{} logged in", user.name),
                "SessionStarted",
                Some(&user.id.0.simple().to_string()),
                Some("pharos"),
            );
        }
        Err(e) => tracing::info!(
            transport = transport.label(),
            outcome = outcome.label(),
            client_ip = %client_ip,
            error = ?e,
            "native login refused"
        ),
    }
    result
}

/// Revokes every session token `user` holds (API keys excepted) but `keep_token`; announces the user
/// when any went and logs how many. Returns the count.
pub async fn revoke_other_tokens(
    state: &AppState,
    user: UserId,
    keep_token: Option<&str>,
    cause: RevokeCause,
) -> Result<u64, AuthError> {
    let revoked = state
        .stores
        .revoke_sessions_except(user, keep_token)
        .await?;
    tracing::info!(
        user_id = %user.0,
        cause = cause.label(),
        revoked,
        "auth: user tokens revoked"
    );
    if revoked > 0 {
        state.revocations.announce(user, cause);
    }
    Ok(revoked)
}

/// Why [`quick_connect_login`] issued no token.
#[derive(Debug)]
pub enum QuickConnectLoginError {
    /// No live request has this secret: never issued, expired, or already
    /// signed in.
    Unknown,
    /// The code has not been authorized yet; the client polls again.
    Pending,
    /// The authorizing user is disabled (V176); the code is spent.
    Disabled,
    /// `admit` refused the authorizing user (a connection cap); the code is
    /// kept, so a retry can still sign in.
    Capped { held: u32, max: u32 },
    /// The registry, user store or token store failed.
    Unavailable(String),
}

impl QuickConnectLoginError {
    pub fn outcome(&self) -> LoginOutcome {
        match self {
            Self::Unknown | Self::Disabled => LoginOutcome::Invalid,
            Self::Pending => LoginOutcome::Pending,
            Self::Capped { .. } => LoginOutcome::Capped,
            Self::Unavailable(_) => LoginOutcome::Unavailable,
        }
    }
}

/// A native Quick Connect sign-in: once a code is authorized, its secret is
/// spent (one-shot, as the Jellyfin finalize) and a token is issued to the
/// authorizer under the device id the request was initiated with. A
/// disabled authorizer gets none (V176). Counted under
/// `method="quick_connect"` and logged, except while the code is pending.
pub async fn quick_connect_login(
    state: &AppState,
    transport: LoginTransport,
    secret: &str,
    admit: impl FnOnce(&User) -> Result<(), (u32, u32)>,
) -> Result<(User, pharos_core::AuthToken), QuickConnectLoginError> {
    use crate::quick_connect::QcMsg;
    use QuickConnectLoginError as E;
    let result: Result<_, E> = async {
        let ask = |consume: bool| async move {
            let (reply, rx) = tokio::sync::oneshot::channel();
            let secret = secret.to_owned();
            let msg = if consume {
                QcMsg::Consume { secret, reply }
            } else {
                QcMsg::Connect { secret, reply }
            };
            state
                .quick_connect
                .tx
                .send(msg)
                .await
                .map_err(|e| E::Unavailable(e.to_string()))?;
            rx.await.map_err(|e| E::Unavailable(e.to_string()))
        };
        // A read first, so a pending code is told apart from an unknown one.
        let entry = ask(false).await?.ok_or(E::Unknown)?;
        let by = entry.authorized_by.ok_or(E::Pending)?;
        let record = state
            .stores
            .get(by)
            .await
            .map_err(|e| E::Unavailable(e.to_string()))?;
        if record.policy.is_disabled {
            // Spend the code, as the Jellyfin finalize does.
            if let Err(e) = ask(true).await {
                tracing::warn!(error = ?e, "quick connect: could not spend a disabled user's code");
            }
            return Err(E::Disabled);
        }
        let user = record.into_user();
        admit(&user).map_err(|(held, max)| E::Capped { held, max })?;
        // One-shot: of two racing polls, only one consumes the entry.
        let entry = ask(true).await?.ok_or(E::Unknown)?;
        let token = state
            .stores
            .issue(user.id, &entry.device_id)
            .await
            .map_err(|e| E::Unavailable(e.to_string()))?;
        Ok((user, token))
    }
    .await;
    match &result {
        Err(E::Pending) => {}
        Ok((user, _)) => {
            record_login(transport, "quick_connect", LoginOutcome::Ok);
            tracing::info!(
                transport = transport.label(),
                user = %user.name,
                "native quick connect login"
            );
            state.record_activity(
                &format!("{} logged in via Quick Connect", user.name),
                "SessionStarted",
                Some(&user.id.0.simple().to_string()),
                Some("pharos"),
            );
        }
        Err(e) => {
            record_login(transport, "quick_connect", e.outcome());
            tracing::info!(
                transport = transport.label(),
                outcome = e.outcome().label(),
                error = ?e,
                "native quick connect login refused"
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// B216: a store that could not answer is not a bad token.
    #[test]
    fn a_backend_failure_is_unavailable_and_anything_else_is_the_stage_refusal() {
        assert_eq!(
            classify(
                AuthError::Backend("pool timed out".into()),
                TokenRefusal::InvalidToken
            ),
            TokenRefusal::Unavailable("pool timed out".into())
        );
        for e in [
            AuthError::InvalidToken,
            AuthError::UserNotFound,
            AuthError::InvalidCredentials,
        ] {
            assert_eq!(
                classify(e, TokenRefusal::UserRevoked),
                TokenRefusal::UserRevoked
            );
        }
    }

    #[test]
    fn login_labels_are_distinct() {
        let outcomes = [
            LoginOutcome::Ok,
            LoginOutcome::Invalid,
            LoginOutcome::Throttled,
            LoginOutcome::Unavailable,
            LoginOutcome::OriginRefused,
            LoginOutcome::Capped,
            LoginOutcome::Pending,
        ];
        let unique: std::collections::HashSet<_> = outcomes.iter().map(|o| o.label()).collect();
        assert_eq!(unique.len(), outcomes.len());
        assert_ne!(LoginTransport::Cookie.label(), LoginTransport::Rpc.label());
    }

    #[test]
    fn revoke_cause_labels_are_distinct() {
        let causes = [
            RevokeCause::Logout,
            RevokeCause::DeviceDeleted,
            RevokeCause::ApiKeyRevoked,
            RevokeCause::UserDeleted,
            RevokeCause::PolicyChanged,
            RevokeCause::PasswordChanged,
        ];
        let unique: std::collections::HashSet<_> = causes.iter().map(|c| c.label()).collect();
        assert_eq!(unique.len(), causes.len());
    }

    /// The announcement is queryable: an operator can see revocations happen
    /// without a native connection open to notice them.
    #[test]
    fn an_announcement_is_counted_by_cause_and_reaches_subscribers() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let bus = Revocations::default();
        let mut rx = bus.subscribe();
        let user = UserId::new();
        metrics::with_local_recorder(&recorder, || bus.announce(user, RevokeCause::Logout));
        assert_eq!(rx.try_recv().unwrap(), user);
        let counted = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| {
                k.key().name() == "pharos_auth_revocation_announced_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "cause" && l.value() == "logout")
            });
        assert!(
            matches!(counted, Some((.., DebugValue::Counter(1)))),
            "{counted:?}"
        );
    }

    #[test]
    fn refusal_labels_are_distinct() {
        let labels = [
            TokenRefusal::InvalidToken.label(),
            TokenRefusal::UserRevoked.label(),
            TokenRefusal::Unavailable(String::new()).label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    #[actix_web::test]
    async fn an_unknown_token_is_invalid() {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        assert_eq!(
            user_for_token(&stores, "nope").await.unwrap_err(),
            TokenRefusal::InvalidToken
        );
    }

    #[actix_web::test]
    async fn a_disabled_users_token_is_refused() {
        use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let hash = crate::auth::BuiltinAuth::new(stores.clone())
            .hash_password(&SecretString::new("p"))
            .unwrap();
        let id = UserId::new();
        stores
            .create(UserRecord {
                id,
                name: "off".into(),
                password_hash: hash,
                policy: UserPolicy {
                    is_disabled: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let token = stores.issue(id, "dev").await.unwrap();
        assert_eq!(
            user_for_token(&stores, token.0.expose()).await.unwrap_err(),
            TokenRefusal::UserRevoked
        );
    }
}
