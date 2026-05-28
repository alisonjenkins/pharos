//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::{
    auth::BuiltinAuth, hls_cache::HlsSegmentCache, image_cache::ImageCache,
    live_tv::M3uXmltvBackend, sessions::SessionRegistry,
    transcode_sessions::TranscodeSessionRegistry,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::PathBuf;
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
    UserDataChanged { user_id: String, item_id: String },
    /// Remote-control command targeted at a single session.
    /// T-fix-17 / T40 phase 2 — admin or another client tells session
    /// `session_id` to pause / play / stop / seek / change volume.
    /// `command` is the Jellyfin PlayState/Command name; `arg` is
    /// freeform JSON the receiving client interprets per command.
    SessionCommand {
        session_id: String,
        command: String,
        arg: serde_json::Value,
    },
}

pub struct AppState {
    pub stores: Stores,
    pub auth: Auth,
    pub sessions: SessionRegistry,
    /// Per-play-session transcode negotiation cache (T-fix-2 part 2).
    /// Populated by `playback_info`; read by HLS segment handler so
    /// segments honour the negotiated codec/container/bitrate.
    pub transcode_sessions: TranscodeSessionRegistry,
    pub images: Option<ImageCache>,
    pub hls: Option<HlsSegmentCache>,
    pub live_tv: Option<M3uXmltvBackend>,
    pub server_id: String,
    pub server_name: String,
    pub version: &'static str,
    /// Configured media roots — same list the CLI `pharos scan`
    /// walks. Held here so admin endpoints (`/Library/Refresh`) can
    /// spawn a real background scan without re-parsing config.
    pub media_roots: Vec<PathBuf>,
    /// Directory pharos surfaces log files from for the
    /// `/System/Logs` admin endpoint. None disables the surface.
    pub log_dir: Option<PathBuf>,
    /// T-fix-Q1 — QuickConnect pending-request registry. Always
    /// available; the `/QuickConnect/Enabled` flag advertises true.
    pub quick_connect: crate::quick_connect::QuickConnectRegistry,
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
        let transcode_sessions = TranscodeSessionRegistry::spawn();
        let (bus, _) = broadcast::channel(256);
        Self {
            stores,
            auth,
            sessions,
            transcode_sessions,
            images: None,
            hls: None,
            live_tv: None,
            media_roots: Vec::new(),
            log_dir: None,
            quick_connect: crate::quick_connect::QuickConnectRegistry::spawn(),
            server_id: Uuid::new_v4().simple().to_string(),
            server_name,
            version: env!("CARGO_PKG_VERSION"),
            bus,
        }
    }

    /// Builder: attach the log-files directory the
    /// `/System/Logs` admin endpoint surfaces.
    pub fn with_log_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.log_dir = dir;
        self
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
        let transcode_sessions = TranscodeSessionRegistry::spawn();
        let (bus, _) = broadcast::channel(256);
        Ok(Self {
            stores,
            auth,
            sessions,
            transcode_sessions,
            images: None,
            hls: None,
            live_tv: None,
            media_roots: Vec::new(),
            log_dir: None,
            quick_connect: crate::quick_connect::QuickConnectRegistry::spawn(),
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

    pub fn with_media_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.media_roots = roots;
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

    /// Fire a `SessionCommand` event for one target session.
    /// Receivers ignore commands not addressed to them.
    pub fn notify_session_command(&self, session_id: &str, command: &str, arg: serde_json::Value) {
        let _ = self.bus.send(SocketBroadcast::SessionCommand {
            session_id: session_id.to_string(),
            command: command.to_string(),
            arg,
        });
    }
}
