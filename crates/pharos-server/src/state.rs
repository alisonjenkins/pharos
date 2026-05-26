//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::{
    auth::BuiltinAuth, hls_cache::HlsSegmentCache, image_cache::ImageCache,
    live_tv::M3uXmltvBackend, sessions::SessionRegistry,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use tokio::sync::broadcast;
use uuid::Uuid;

pub type Stores = SqliteStore;
pub type Auth = BuiltinAuth<Stores>;

/// Server-originated notifications fanned out to every connected
/// `/socket`. T40 phase 2 — keeps client UIs (jellyfin-web especially)
/// in sync with library + per-user state without polling.
#[derive(Debug, Clone)]
pub enum SocketBroadcast {
    /// Library content changed (item added/updated/removed).
    /// jellyfin-web treats this as a hint to invalidate its item
    /// cache and refresh visible views.
    LibraryChanged,
    /// Per-user item state changed (played, favourite, position).
    /// Carries the originating user + item so receivers can ignore
    /// updates that don't apply to them.
    UserDataChanged {
        user_id: String,
        item_id: String,
    },
}

pub struct AppState {
    pub stores: Stores,
    pub auth: Auth,
    pub sessions: SessionRegistry,
    pub images: Option<ImageCache>,
    pub hls: Option<HlsSegmentCache>,
    pub live_tv: Option<M3uXmltvBackend>,
    pub server_id: String,
    pub server_name: String,
    pub version: &'static str,
    /// Broadcast bus used by `/socket`. Capacity 256 — bursts during
    /// a library refresh stay buffered; slow consumers see a Lagged
    /// signal which `socket.rs` translates into "drop + re-subscribe".
    pub bus: broadcast::Sender<SocketBroadcast>,
}

impl AppState {
    /// Construct with a fresh random `server_id`. Reserved for tests that
    /// don't care about identity persistence — production callers should
    /// use [`AppState::load`] so jellyfin clients don't re-pair across
    /// restarts (T35).
    pub fn new(stores: Stores, server_name: String) -> Self {
        let auth = BuiltinAuth::new(stores.clone());
        let sessions = SessionRegistry::spawn();
        let (bus, _) = broadcast::channel(256);
        Self {
            stores,
            auth,
            sessions,
            images: None,
            hls: None,
            live_tv: None,
            server_id: Uuid::new_v4().simple().to_string(),
            server_name,
            version: env!("CARGO_PKG_VERSION"),
            bus,
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
        let (bus, _) = broadcast::channel(256);
        Ok(Self {
            stores,
            auth,
            sessions,
            images: None,
            hls: None,
            live_tv: None,
            server_id,
            server_name,
            version: env!("CARGO_PKG_VERSION"),
            bus,
        })
    }

    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.images = Some(cache);
        self
    }

    pub fn with_hls_cache(mut self, cache: HlsSegmentCache) -> Self {
        self.hls = Some(cache);
        self
    }

    pub fn with_live_tv(mut self, backend: M3uXmltvBackend) -> Self {
        self.live_tv = Some(backend);
        self
    }

    /// Fire a `LibraryChanged` event to every connected `/socket`.
    /// No-op when there are zero subscribers (broadcast::send returns
    /// Err but we don't care).
    pub fn notify_library_changed(&self) {
        let _ = self.bus.send(SocketBroadcast::LibraryChanged);
    }

    /// Fire a `UserDataChanged` event scoped to one user + item.
    pub fn notify_user_data_changed(&self, user_id: &str, item_id: &str) {
        let _ = self.bus.send(SocketBroadcast::UserDataChanged {
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
        });
    }
}
