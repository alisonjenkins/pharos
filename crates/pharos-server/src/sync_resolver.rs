//! Adapter wiring `pharos_sync::TokenResolver` to the server's
//! concrete `TokenStore` impl. Lifts the WS handler out of the
//! `AppState` blast radius — `pharos-sync` only sees this thin
//! trait, not the full server state.

use crate::state::Stores;
use pharos_core::{SecretString, TokenStore, UserId};
use pharos_sync::TokenResolver;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct StoreTokenResolver(Stores);

impl TokenResolver for StoreTokenResolver {
    fn resolve<'a>(
        &'a self,
        token: &'a SecretString,
    ) -> Pin<Box<dyn Future<Output = Option<UserId>> + Send + 'a>> {
        Box::pin(async move { self.0.resolve(token.expose()).await.ok() })
    }
}

/// Wrap the server's concrete `Stores` as an `Arc<dyn TokenResolver>`
/// ready for `actix_web::web::Data::new(_)`. Production wiring (main.rs)
/// and integration tests share this helper so the resolver impl stays
/// in one place.
pub fn build(stores: Stores) -> Arc<dyn TokenResolver> {
    Arc::new(StoreTokenResolver(stores))
}
