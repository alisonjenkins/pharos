//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::{auth::BuiltinAuth, image_cache::ImageCache, sessions::SessionRegistry};
use pharos_store_sqlx::sqlite::SqliteStore;
use uuid::Uuid;

pub type Stores = SqliteStore;
pub type Auth = BuiltinAuth<Stores>;

pub struct AppState {
    pub stores: Stores,
    pub auth: Auth,
    pub sessions: SessionRegistry,
    pub images: Option<ImageCache>,
    pub server_id: String,
    pub server_name: String,
    pub version: &'static str,
}

impl AppState {
    /// Construct with a fresh random `server_id`. Reserved for tests that
    /// don't care about identity persistence — production callers should
    /// use [`AppState::load`] so jellyfin clients don't re-pair across
    /// restarts (T35).
    pub fn new(stores: Stores, server_name: String) -> Self {
        let auth = BuiltinAuth::new(stores.clone());
        let sessions = SessionRegistry::spawn();
        Self {
            stores,
            auth,
            sessions,
            images: None,
            server_id: Uuid::new_v4().simple().to_string(),
            server_name,
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Construct from a store, reading or initialising the persistent
    /// `server_id` from `system_identity`. Same id returned across
    /// restarts.
    pub async fn load(
        stores: Stores,
        server_name: String,
    ) -> Result<Self, pharos_store_sqlx::StoreError> {
        let server_id = stores.load_or_create_server_id().await?;
        let auth = BuiltinAuth::new(stores.clone());
        let sessions = SessionRegistry::spawn();
        Ok(Self {
            stores,
            auth,
            sessions,
            images: None,
            server_id,
            server_name,
            version: env!("CARGO_PKG_VERSION"),
        })
    }

    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.images = Some(cache);
        self
    }
}
