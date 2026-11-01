//! Token to [`User`]: the decision every authenticated call on either API
//! makes, implemented once (V172).

use pharos_core::{AuthError, TokenStore, User, UserStore};

use crate::state::Stores;

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
