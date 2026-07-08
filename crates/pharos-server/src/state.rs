//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::{
    auth::BuiltinAuth, sessions::SessionRegistry, transcode_sessions::TranscodeSessionRegistry,
};
use pharos_cache::{HlsSegmentCache, ImageCache, SubtitleCache, TrickplayCache};
use pharos_discovery::live_tv::M3uXmltvBackend;
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_transcode::FfmpegBackend;
use std::path::PathBuf;
use std::sync::Arc;
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
    ///
    /// LIB-A4 — carries the affected item ids (rendered as the decimal
    /// strings clients use as Jellyfin item ids) so the wire payload can
    /// populate `ItemsAdded` / `ItemsRemoved`. Both empty is still valid
    /// (a generic "something changed" hint, e.g. an image upload).
    LibraryChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
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
    /// P10 — playback progress update fans out so jellyfin-web's
    /// "Currently Watching" sidebar + remote-control UIs reflect the
    /// active position without polling. Fired by
    /// `/Sessions/Playing/Progress`.
    PlaybackProgress {
        session_id: String,
        user_id: String,
        item_id: String,
        position_ticks: u64,
        is_paused: bool,
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
    /// Load-balancing transcode scheduler (multi-GPU + all-CPU). When
    /// present, the live/uncached HLS path streams through it; the cached
    /// path uses its own clone held inside `HlsSegmentCache`.
    pub transcode_scheduler: Option<pharos_transcode::scheduler::TranscodeScheduler>,
    pub trickplay: Option<TrickplayCache>,
    pub subtitles: Option<SubtitleCache>,
    /// Trickplay layout knobs surfaced to handlers + DTO emitter so
    /// the wire shape matches what was actually generated.
    pub trickplay_widths: Vec<u32>,
    pub trickplay_interval_ms: u32,
    /// Nudge the background trickplay pre-generator to prioritise an item's
    /// whole series (PlaybackInfo sends the played item id). None when
    /// trickplay is disabled.
    pub trickplay_priority: Option<crate::trickplay_backfill::PriorityTx>,
    pub live_tv: Option<M3uXmltvBackend>,
    pub server_id: String,
    pub server_name: String,
    pub version: &'static str,
    /// Configured media roots — same list the CLI `pharos scan`
    /// walks. Held here so admin endpoints (`/Library/Refresh`) can
    /// spawn a real background scan without re-parsing config.
    pub media_roots: Vec<PathBuf>,
    /// LIB-C1 — typed libraries reconciled from `[media]` config at boot
    /// (one per configured root, with its kind + stable wire id). Drives
    /// `/Library/VirtualFolders` + `/Library/MediaFolders` +
    /// `/Users/{u}/Views` so they advertise the real per-root
    /// CollectionType instead of the legacy single "All Media / mixed"
    /// stub. Empty → the views fall back to synthesising one `mixed`
    /// library per `media_roots` entry (tests that only call
    /// `with_media_roots`), and to the all-zeros placeholder when there
    /// are no roots either.
    /// Wrapped in an `RwLock` so the dashboard's Add/Remove-library
    /// endpoints (`POST`/`DELETE /Library/VirtualFolders`) can reconcile the
    /// set at runtime without a restart. Read via [`AppState::libraries`];
    /// replaced via [`AppState::set_libraries`].
    pub library_set: Arc<std::sync::RwLock<Vec<pharos_core::Library>>>,
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
    /// P36 — clamped played-flag threshold (50–100) used by
    /// `Sessions/Playing/Stopped` to decide when an item flips to
    /// `played=true`. Surfaced here so handlers stay zero-allocation
    /// per-request.
    pub played_threshold_pct: u32,
    /// P43 — inter-probe sleep in milliseconds for background
    /// `/Library/Refresh` passes. 0 disables rate-limiting. Surfaced
    /// here so the admin spawn reads the configured value without
    /// re-parsing the toml config.
    pub scan_rate_limit_ms: u64,
    /// P48 — ffmpeg operations backend. `Arc<dyn FfmpegBackend>` so
    /// the spawn / lib-FFI swap happens at construction time without
    /// rippling generic parameters through every handler signature.
    /// Default at `AppState::new` is the spawn backend so tests get
    /// the production behaviour without extra wiring.
    pub ffmpeg: Arc<dyn FfmpegBackend>,
    /// Memoises a synthesised Series/Season/Artist/Album wire id → the
    /// representative member item id whose frame/cover is its poster. Without
    /// this, every synth-item image request would re-scan the whole library
    /// (`list()`), and a TV-library grid fires one per visible tile. `None`
    /// caches a negative (id matched no group) so misses don't rescan either.
    pub synth_image_ids: Arc<std::sync::Mutex<std::collections::HashMap<String, Option<u64>>>>,
}

impl AppState {
    /// Look up a memoised synth-id → representative item id, if present.
    pub fn synth_image_cached(&self, id: &str) -> Option<Option<u64>> {
        self.synth_image_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()
    }

    /// Record a synth-id → representative item id resolution.
    pub fn synth_image_remember(&self, id: &str, item_id: Option<u64>) {
        self.synth_image_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), item_id);
    }

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
            transcode_scheduler: None,
            trickplay: None,
            subtitles: None,
            trickplay_widths: Vec::new(),
            trickplay_interval_ms: 10_000,
            trickplay_priority: None,
            live_tv: None,
            media_roots: Vec::new(),
            library_set: Arc::new(std::sync::RwLock::new(Vec::new())),
            log_dir: None,
            quick_connect: crate::quick_connect::QuickConnectRegistry::spawn(),
            server_id: Uuid::new_v4().simple().to_string(),
            server_name,
            version: env!("CARGO_PKG_VERSION"),
            bus,
            played_threshold_pct: 90,
            scan_rate_limit_ms: 0,
            ffmpeg: default_ffmpeg_backend(),
            synth_image_ids: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Builder: attach the log-files directory the
    /// `/System/Logs` admin endpoint surfaces.
    pub fn with_log_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.log_dir = dir;
        self
    }

    /// Resolve the active `(server_name, login_disclaimer, custom_css)`
    /// triple. Reads `runtime_config` once per call; the override wins
    /// over the toml-supplied defaults. Returns `None` for fields with
    /// no override so callers can fall through to their own defaults.
    pub async fn effective_branding(&self) -> pharos_store_sqlx::RuntimeConfig {
        self.stores.load_runtime_config().await.unwrap_or_default()
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
            transcode_scheduler: None,
            trickplay: None,
            subtitles: None,
            trickplay_widths: Vec::new(),
            trickplay_interval_ms: 10_000,
            trickplay_priority: None,
            live_tv: None,
            media_roots: Vec::new(),
            library_set: Arc::new(std::sync::RwLock::new(Vec::new())),
            log_dir: None,
            quick_connect: crate::quick_connect::QuickConnectRegistry::spawn(),
            server_id,
            server_name,
            version: env!("CARGO_PKG_VERSION"),
            bus,
            played_threshold_pct: 90,
            scan_rate_limit_ms: 0,
            ffmpeg: default_ffmpeg_backend(),
            synth_image_ids: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// P36 builder — apply the configured played-threshold,
    /// clamping to `[50, 100]` so a misconfigured 0 doesn't
    /// flip every play to played=true and a 250 doesn't make
    /// played unreachable.
    pub fn with_played_threshold_pct(mut self, pct: u32) -> Self {
        self.played_threshold_pct = pct.clamp(50, 100);
        self
    }

    /// P43 builder — apply the configured per-probe rate-limit for
    /// background library refresh. Capped at 5 seconds so a typo
    /// can't make a refresh run effectively forever.
    pub fn with_scan_rate_limit_ms(mut self, ms: u64) -> Self {
        self.scan_rate_limit_ms = ms.min(5_000);
        self
    }

    /// P48 builder — install a custom `FfmpegBackend`. Tests use
    /// this to short-circuit real ffmpeg invocations entirely;
    /// production wiring picks `SpawnBackend` or `LibBackend` via
    /// the `pharos-transcode` cargo features.
    pub fn with_ffmpeg_backend(mut self, backend: Arc<dyn FfmpegBackend>) -> Self {
        self.ffmpeg = backend;
        self
    }

    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.images = Some(cache);
        self
    }

    pub fn with_hls_cache(mut self, cache: HlsSegmentCache) -> Self {
        self.hls = Some(cache);
        self
    }

    pub fn with_transcode_scheduler(
        mut self,
        sched: pharos_transcode::scheduler::TranscodeScheduler,
    ) -> Self {
        self.transcode_scheduler = Some(sched);
        self
    }

    pub fn with_trickplay_cache(mut self, cache: TrickplayCache) -> Self {
        self.trickplay = Some(cache);
        self
    }

    pub fn with_subtitle_cache(mut self, cache: SubtitleCache) -> Self {
        self.subtitles = Some(cache);
        self
    }

    pub fn with_trickplay_layout(mut self, widths: Vec<u32>, interval_ms: u32) -> Self {
        self.trickplay_widths = widths;
        self.trickplay_interval_ms = interval_ms.max(1_000);
        self
    }

    pub fn with_trickplay_priority(mut self, tx: crate::trickplay_backfill::PriorityTx) -> Self {
        self.trickplay_priority = Some(tx);
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

    /// LIB-C1 builder — install the typed libraries reconciled from
    /// config. When set, `/Library/VirtualFolders` + `/Library/MediaFolders`
    /// and the view list advertise these (with per-kind CollectionType)
    /// instead of synthesising `mixed` libraries from `media_roots`.
    pub fn with_libraries(self, libraries: Vec<pharos_core::Library>) -> Self {
        self.set_libraries(libraries);
        self
    }

    /// Read snapshot of the typed libraries. Poison-safe (a panicked writer
    /// leaves the data intact; we read through the poison rather than
    /// propagating a panic into request handling).
    pub fn libraries(&self) -> std::sync::RwLockReadGuard<'_, Vec<pharos_core::Library>> {
        self.library_set
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the typed-library set at runtime (dashboard Add/Remove).
    pub fn set_libraries(&self, libraries: Vec<pharos_core::Library>) {
        *self
            .library_set
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = libraries;
    }

    /// Fire a `LibraryChanged` event to every connected `/socket`.
    /// No-op when there are zero subscribers (broadcast::send returns
    /// Err but we don't care).
    pub fn notify_library_changed(&self) {
        let _ = self.bus.send(SocketBroadcast::LibraryChanged {
            added: Vec::new(),
            removed: Vec::new(),
        });
    }

    /// LIB-A4 — fire a `LibraryChanged` event carrying the item-id deltas
    /// from a scan so connected `/socket` clients can surgically refresh
    /// (ItemsAdded / ItemsRemoved) rather than invalidating their whole
    /// cache. `added` / `removed` are the [`pharos_core::MediaId`]s the
    /// scan produced, rendered as the decimal strings clients use as
    /// Jellyfin item ids. No-op when there are zero subscribers.
    pub fn notify_library_delta(
        &self,
        added: &[pharos_core::MediaId],
        removed: &[pharos_core::MediaId],
    ) {
        let _ = self.bus.send(SocketBroadcast::LibraryChanged {
            added: added.iter().map(|id| id.to_string()).collect(),
            removed: removed.iter().map(|id| id.to_string()).collect(),
        });
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

    /// P10 — fan out a `PlaybackProgress` event so connected `/socket`
    /// subscribers can update their Currently Watching UI without
    /// polling. Fired from `/Sessions/Playing/Progress`.
    pub fn notify_playback_progress(
        &self,
        session_id: &str,
        user_id: &str,
        item_id: &str,
        position_ticks: u64,
        is_paused: bool,
    ) {
        let _ = self.bus.send(SocketBroadcast::PlaybackProgress {
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            position_ticks,
            is_paused,
        });
    }
}

/// P48 — produce the compile-time-selected default backend. The
/// feature flags on `pharos-transcode` are mutually exclusive at
/// link time (build script enforces); pick whichever feature is
/// enabled. Tests + main both share this path so swap behaviour
/// stays consistent.
fn default_ffmpeg_backend() -> Arc<dyn FfmpegBackend> {
    #[cfg(feature = "ffmpeg-lib")]
    {
        Arc::new(pharos_transcode::LibBackend::new())
    }
    #[cfg(not(feature = "ffmpeg-lib"))]
    {
        Arc::new(pharos_transcode::SpawnBackend::new())
    }
}
