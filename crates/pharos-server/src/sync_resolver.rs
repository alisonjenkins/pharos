//! Adapter wiring `pharos_sync::TokenResolver` to the server's
//! concrete stores and access checks. Lifts the WS handler out of the
//! `AppState` blast radius — `pharos-sync` only sees this thin
//! trait, not the full server state.

use crate::state::AppState;
use pharos_core::{SecretString, TokenScope, TokenStore, UserId, UserStore};
use pharos_sync::group::GroupHandle;
use pharos_sync::registry::RegistryError;
use pharos_sync::GroupRegistry;
use pharos_sync::TokenResolver;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct StoreTokenResolver(Arc<AppState>);

impl StoreTokenResolver {
    /// The user a socket authenticated as, with their current policy.
    async fn user(&self, id: UserId) -> Option<pharos_core::User> {
        match self.0.stores.get(id).await {
            Ok(record) => Some(record.into_user()),
            Err(e) => {
                tracing::warn!(user = %id.0, error = %e, "sync ws: user not found");
                None
            }
        }
    }
}

impl TokenResolver for StoreTokenResolver {
    fn resolve<'a>(
        &'a self,
        token: &'a SecretString,
    ) -> Pin<Box<dyn Future<Output = Option<UserId>> + Send + 'a>> {
        Box::pin(async move {
            // The SyncPlay socket is not on a playback token's allowlist.
            let resolved = self.0.stores.resolve_scoped(token.expose()).await.ok()?;
            (resolved.scope == TokenScope::Full).then_some(resolved.user)
        })
    }

    fn may_create(&self, user: UserId) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let Some(user) = self.user(user).await else {
                return false;
            };
            crate::api::jellyfin::syncplay::sync_play_allows(
                &user,
                crate::api::jellyfin::syncplay::SyncPlayNeed::Create,
                "native",
            )
        })
    }

    fn joinable_group<'a>(
        &'a self,
        user: UserId,
        registry: &'a GroupRegistry,
        group_id: pharos_sync::messages::GroupId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<GroupHandle>, RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(user) = self.user(user).await else {
                return Ok(None);
            };
            crate::sync_recovery::joinable_group(
                &self.0,
                registry,
                &user,
                group_id,
                crate::api::jellyfin::syncplay::GroupHiddenRoute::Native,
            )
            .await
        })
    }
}

/// Wrap the server's state as an `Arc<dyn TokenResolver>` ready for
/// `actix_web::web::Data::new(_)`. Production wiring (main.rs) and
/// integration tests share this helper so the resolver impl stays in one
/// place.
pub fn build(state: Arc<AppState>) -> Arc<dyn TokenResolver> {
    Arc::new(StoreTokenResolver(state))
}
