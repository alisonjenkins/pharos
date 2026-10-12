//! Cast: TV-bound access tokens.
//!
//! The stock Chromecast receiver calls pharos back with a token we hand it,
//! its own `DeviceId` and no user id, so the token must resolve the owner on
//! its own. It goes through the normal token path ([`TokenStore`]): hashed at
//! rest, expiring, revocable.

use pharos_core::{AuthError, AuthToken, SecretString, TokenId, TokenScope, TokenStore, UserId};
use std::time::Duration;

/// How long a TV token lives when the caller has no better bound: one
/// evening's viewing plus slack, far shorter than a login token's 90 days.
pub const DEFAULT_TV_TOKEN_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// Device name a TV token is stored under, so it shows up as a cast token in
/// the device list and can be revoked by device.
fn tv_device_id(tv_name: &str) -> String {
    format!("cast:{tv_name}")
}

/// A minted TV token. `id` is safe to keep and log; the secret is shown to
/// the receiver once and never printed (V8).
#[derive(Clone)]
pub struct TvToken {
    pub id: TokenId,
    secret: AuthToken,
}

impl TvToken {
    pub fn secret(&self) -> &SecretString {
        &self.secret.0
    }
}

impl std::fmt::Debug for TvToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TvToken")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CastTokenError {
    #[error("cannot mint a cast token for an empty TV name")]
    EmptyTvName,
    #[error("cannot mint a cast token for TV {tv_name:?}: lifetime {lifetime:?} is not a positive, representable duration")]
    InvalidLifetime { tv_name: String, lifetime: Duration },
    /// The token store could not answer; transient, not a bad credential.
    #[error("token store unavailable while minting a cast token for user {user} on TV {tv_name:?}: {source}")]
    MintStoreUnavailable {
        user: String,
        tv_name: String,
        #[source]
        source: AuthError,
    },
    #[error("token store refused a cast token for user {user} on TV {tv_name:?}: {source}")]
    MintRejected {
        user: String,
        tv_name: String,
        #[source]
        source: AuthError,
    },
    #[error("token store unavailable while revoking cast token {token_id}: {source}")]
    RevokeStoreUnavailable {
        token_id: String,
        #[source]
        source: AuthError,
    },
    #[error("token store refused to revoke cast token {token_id}: {source}")]
    RevokeRejected {
        token_id: String,
        #[source]
        source: AuthError,
    },
}

impl CastTokenError {
    /// True when the failure is the store being unreachable (map to 503 /
    /// retry), not anything wrong with the caller's input or credentials (V165).
    pub fn is_dependency_failure(&self) -> bool {
        matches!(
            self,
            Self::MintStoreUnavailable { .. } | Self::RevokeStoreUnavailable { .. }
        )
    }
}

/// Mint an access token for `user`, bound to the synthetic device
/// `cast:<tv_name>`, that expires after `lifetime`.
// T052: the token store has no scope field, so this token carries the owner's
// full permissions until a playback-only scope exists.
pub async fn mint_tv_token<S: TokenStore>(
    store: &S,
    user: UserId,
    tv_name: &str,
    lifetime: Duration,
) -> Result<TvToken, CastTokenError> {
    if tv_name.trim().is_empty() {
        return Err(CastTokenError::EmptyTvName);
    }
    let ttl_secs = i64::try_from(lifetime.as_secs())
        .ok()
        .filter(|s| *s > 0)
        .ok_or_else(|| CastTokenError::InvalidLifetime {
            tv_name: tv_name.to_string(),
            lifetime,
        })?;
    match store
        .issue_with_ttl(user, &tv_device_id(tv_name), ttl_secs, TokenScope::Full)
        .await
    {
        Ok(issued) => Ok(TvToken {
            id: issued.id,
            secret: issued.secret,
        }),
        Err(source @ AuthError::Backend(_)) => Err(CastTokenError::MintStoreUnavailable {
            user: user.0.simple().to_string(),
            tv_name: tv_name.to_string(),
            source,
        }),
        Err(source) => Err(CastTokenError::MintRejected {
            user: user.0.simple().to_string(),
            tv_name: tv_name.to_string(),
            source,
        }),
    }
}

/// Revoke a TV token. Idempotent: revoking an unknown or already revoked id
/// succeeds.
pub async fn revoke_tv_token<S: TokenStore>(store: &S, id: &TokenId) -> Result<(), CastTokenError> {
    match store.revoke_by_id(id).await {
        Ok(()) => Ok(()),
        Err(source @ AuthError::Backend(_)) => Err(CastTokenError::RevokeStoreUnavailable {
            token_id: id.0.clone(),
            source,
        }),
        Err(source) => Err(CastTokenError::RevokeRejected {
            token_id: id.0.clone(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn debug_never_shows_the_secret() {
        let tv = TvToken {
            id: TokenId("abc123".into()),
            secret: AuthToken(SecretString::new("super-secret-value")),
        };
        let shown = format!("{tv:?} {tv:#?}");
        assert!(!shown.contains("super-secret-value"), "{shown}");
        assert!(shown.contains("abc123"), "{shown}");
    }

    #[test]
    fn device_id_is_prefixed() {
        assert_eq!(tv_device_id("Den"), "cast:Den");
    }
}
