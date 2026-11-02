//! Adapter wiring `pharos_sync::TokenResolver` to the server's
//! concrete stores and access checks. Lifts the WS handler out of the
//! `AppState` blast radius — `pharos-sync` only sees this thin
//! trait, not the full server state.

use crate::state::AppState;
use pharos_core::{SecretString, TokenStore, UserId, UserStore};
use pharos_sync::group::GroupHandle;
use pharos_sync::TokenResolver;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct StoreTokenResolver(Arc<AppState>);

impl TokenResolver for StoreTokenResolver {
    fn resolve<'a>(
        &'a self,
        token: &'a SecretString,
    ) -> Pin<Box<dyn Future<Output = Option<UserId>> + Send + 'a>> {
        Box::pin(async move { self.0.stores.resolve(token.expose()).await.ok() })
    }

    fn may_join<'a>(
        &'a self,
        user: UserId,
        group: &'a GroupHandle,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let user = match self.0.stores.get(user).await {
                Ok(record) => record.into_user(),
                Err(e) => {
                    tracing::warn!(user = %user.0, error = %e, "sync ws: joining user not found");
                    return false;
                }
            };
            crate::api::jellyfin::syncplay::may_join(
                &self.0,
                &user,
                group,
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
