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
}

impl RevokeCause {
    pub fn label(self) -> &'static str {
        match self {
            RevokeCause::Logout => "logout",
            RevokeCause::DeviceDeleted => "device_deleted",
            RevokeCause::ApiKeyRevoked => "api_key_revoked",
            RevokeCause::UserDeleted => "user_deleted",
            RevokeCause::PolicyChanged => "policy_changed",
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
    fn revoke_cause_labels_are_distinct() {
        let causes = [
            RevokeCause::Logout,
            RevokeCause::DeviceDeleted,
            RevokeCause::ApiKeyRevoked,
            RevokeCause::UserDeleted,
            RevokeCause::PolicyChanged,
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
