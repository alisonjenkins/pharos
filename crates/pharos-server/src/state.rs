//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::{
    auth::BuiltinAuth, sessions::SessionRegistry, transcode_sessions::TranscodeSessionRegistry,
};
use pharos_cache::{HlsSegmentCache, ImageCache, SubtitleCache, TrickplayCache};
use pharos_discovery::live_tv::M3uXmltvBackend;
use pharos_store_sqlx::ServerConfigStore;
use pharos_transcode::{FfmpegBackend, SegmentOpts};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

// Concrete store backend, chosen at COMPILE time by the `postgres` feature and
// at RUNTIME by the DB URL scheme:
// - default (sqlite only): the store is `SqliteStore` directly.
// - `--features postgres` (the shipped image): `AnyStore`, a runtime enum that
//   dispatches `sqlite://` vs `postgres://` to the matching backend. Handlers
//   stay free of generics either way — this alias is the only swap point.
#[cfg(feature = "postgres")]
pub type Stores = pharos_store_sqlx::any::AnyStore;
#[cfg(not(feature = "postgres"))]
pub type Stores = pharos_store_sqlx::sqlite::SqliteStore;
pub type Auth = BuiltinAuth<Stores>;

/// Concurrent heavy background file reads permitted when NOTHING is playing
/// (scan probes, subtitle warm-demuxes, trickplay pre-generation) — the `bg_io`
/// semaphore's full size. Sized for parallel background work on a many-core box
/// with a 13k-item NFS library: trickplay generation is I/O-bound (keyframe
/// seeks leave the CPU near-idle), so a low ceiling left the box mostly idle
/// while the backfill crawled. This is only the IDLE ceiling — during playback
/// the regulator parks down to `BG_IO_BUSY`, so live streams are unaffected.
const BG_IO_MAX: usize = 8;
/// …and during active playback: a trickle, so background work keeps making
/// progress but can't saturate NFS out from under a live stream's segment
/// reads. The regulator parks `BG_IO_MAX - BG_IO_BUSY` permits while playing.
const BG_IO_BUSY: usize = 1;
/// A live segment pulled within this many seconds counts as "playing".
const BG_IO_BUSY_WINDOW_SECS: i64 = 12;

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
    /// Carries the originating user plus the FULL serialized
    /// `UserItemDataDto` entries (B36) — jellyfin-web patches cards in
    /// place from `UserDataList[n].Played` / `.IsFavorite` / matched by
    /// `.ItemId` (canonical 32-hex) and `.Key`, so a bare item id is
    /// useless to it. One broadcast may carry many entries (folder
    /// cascade marks every child episode in a single frame).
    UserDataChanged {
        user_id: String,
        entries: Vec<serde_json::Value>,
    },
    /// Remote-control command targeted at a single session.
    /// T-fix-17 / T40 phase 2 — admin or another client tells session
    /// `session_id` to pause / play / stop / seek / change volume.
    /// `command` is the Jellyfin PlayState/Command name; `arg` is
    /// freeform JSON the receiving client interprets per command.
    SessionCommand {
        session_id: String,
        /// The user issuing the command — becomes the wire `ControllingUserId`
        /// (a NON-null UUID for GeneralCommand / PlayRequest; B79). 32-hex form.
        controlling_user_id: String,
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
        /// Resolved kind of `item_id`, so the `Sessions` broadcast can stamp
        /// the kotlin-REQUIRED `NowPlayingItem.Type`. `None` when the item
        /// couldn't be resolved — the broadcast then omits `NowPlayingItem`
        /// rather than emit a `Type`-less one that crashes strict clients.
        item_kind: Option<pharos_core::MediaKind>,
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
    pub transcode_sessions: TranscodeSessionRegistry<Stores>,
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
    /// T87 — the last segment-transcode options each PlaySessionId used,
    /// keyed by play session. Lets the SyncPlay seek handler PREWARM the
    /// target segments for every group member's exact variant (audio pick,
    /// burned sub, codec) the moment the Seek command is dispatched — the
    /// server knows the target seconds before any client applies the seek
    /// at `When` and requests data. Bounded small (one entry per live play
    /// session); cleared wholesale past a sanity cap.
    pub segment_opts_hints:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (u64, SegmentOpts)>>>,
    /// P36 — clamped played-flag threshold (50–100) used by
    /// `Sessions/Playing/Stopped` to decide when an item flips to
    /// `played=true`. Surfaced here so handlers stay zero-allocation
    /// per-request.
    pub played_threshold_pct: u32,
    /// B60 — when true, desktop Linux Firefox is served H.264 (shared encode)
    /// instead of the forced VP9/WebM path. Config `[server].linux_firefox_h264`.
    pub linux_firefox_h264: bool,
    /// P43 — inter-probe sleep in milliseconds for background
    /// `/Library/Refresh` passes. 0 disables rate-limiting. Surfaced
    /// here so the admin spawn reads the configured value without
    /// re-parsing the toml config.
    pub scan_rate_limit_ms: u64,
    /// #11 — probe fan-out cap for background `/Library/Refresh` scans. 0 keeps
    /// the scanner's conservative default (leaves shared-storage I/O headroom).
    pub scan_probe_concurrency: usize,
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
    /// Serialises the one-time synth-image-map warm (see
    /// `resolve_synth_image_item`). A library grid fires many poster requests
    /// at once for distinct synth ids; without this each would run its own
    /// full-library `list()` (seconds each, 13k rows). The first miss takes
    /// this lock, builds the whole synth-id → representative map in one scan,
    /// and every other request then hits the memo.
    pub synth_image_warm: Arc<tokio::sync::Mutex<()>>,
    /// Unix-seconds of the last live-playback segment request. The
    /// background asset backfill (trickplay / subtitles) reads this and
    /// stands down while a client is actively streaming, so a whole-file
    /// decode never contends with live segment transcoding and makes
    /// playback buffer. Bumped by the HLS + VP9 segment handlers.
    pub playback_activity: Arc<std::sync::atomic::AtomicI64>,
    /// Adaptive backpressure for HEAVY background file reads (scan probes,
    /// subtitle warm-demuxes). Every such op acquires a permit here before
    /// touching the source; live playback does NOT. A regulator task
    /// ([`spawn_bg_io_regulator`](Self::spawn_bg_io_regulator)) parks all but
    /// [`BG_IO_BUSY`] permits while a client is streaming, so background I/O
    /// self-throttles to a trickle (but never stops) and can't starve a live
    /// stream's segment reads — then reopens to [`BG_IO_MAX`] once playback is
    /// quiet. This is what lets a full force-rescan run DURING playback.
    pub bg_io: Arc<tokio::sync::Semaphore>,
    /// Phase B2 — true when this replica holds background-work leadership
    /// (the Postgres advisory lock, or unconditionally under SQLite). The
    /// DB-writing background loops (library rescan, trickplay/subtitle warm,
    /// image janitor) gate on it so exactly one replica runs them during a
    /// rolling-deploy surge. Flipped by [`spawn_bg_leadership`](Self::spawn_bg_leadership).
    pub is_bg_leader: Arc<std::sync::atomic::AtomicBool>,
    /// Monotonic library version, bumped on every library-mutating notify
    /// (`notify_library_changed` / `notify_library_delta`). The full-item-list
    /// cache below keys on this so any add/remove instantly invalidates it.
    pub library_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Single-flight cache of the whole `media_items` list. Home-screen +
    /// synth-id endpoints each did a full ~13k-row `store.list()` (1-4s) per
    /// request, and browsing fired several at once. Holds
    /// `(generation, built_at, items)` — a hit requires the generation to
    /// still match AND the entry to be under `LIST_CACHE_MAX_AGE` (a backstop
    /// so a missed invalidation self-heals). See `list_items_cached`.
    #[allow(clippy::type_complexity)]
    pub item_list_cache: Arc<
        tokio::sync::Mutex<Option<(u64, std::time::Instant, Arc<Vec<pharos_core::MediaItem>>)>>,
    >,
    /// T73 — recent activity entries (logins, etc.) for the dashboard's
    /// Activity panel, newest-first. In-memory ring buffer (session-scoped, not
    /// persisted — the panel wants "recent activity", and a bounded buffer keeps
    /// it cheap); capped at [`ACTIVITY_LOG_CAP`]. Entries are pre-shaped Jellyfin
    /// `ActivityLogEntry` JSON. `next_activity_id` hands out monotonic ids.
    pub activity_log: Arc<std::sync::Mutex<std::collections::VecDeque<serde_json::Value>>>,
    pub next_activity_id: Arc<std::sync::atomic::AtomicI64>,
    /// T97/B72 — in-memory memo + single-flight for computed audio waveforms, so
    /// a re-open / re-seek burst never re-decodes the whole source. Session-
    /// scoped (the endpoint is client-optional); see [`crate::api::jellyfin::waveform`].
    pub waveform: crate::api::jellyfin::waveform::WaveformCache,
    /// T68 — official-rating → score table backing `MaxParentalRating`
    /// enforcement. Defaults to the built-in US table; boot overrides it from
    /// `[parental]` config when present.
    pub parental_ratings: crate::parental::ParentalRatingMap,
}

/// Max recent activity entries retained in memory (T73).
pub const ACTIVITY_LOG_CAP: usize = 250;

/// Unix time in whole seconds (0 if the clock is before the epoch).
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

    /// Stamp "a client just pulled a live segment" — called from the segment
    /// handlers so the background backfill can yield to active playback.
    pub fn note_playback_activity(&self) {
        self.playback_activity
            .store(unix_now_secs(), std::sync::atomic::Ordering::Relaxed);
    }

    /// True when a live segment was pulled within the last [`BG_IO_BUSY_WINDOW_SECS`].
    pub fn playback_active(&self) -> bool {
        let last = self
            .playback_activity
            .load(std::sync::atomic::Ordering::Relaxed);
        last != 0 && unix_now_secs() - last < BG_IO_BUSY_WINDOW_SECS
    }

    /// Spawn the background-I/O regulator: while playback is active it parks
    /// `BG_IO_MAX - BG_IO_BUSY` permits (throttling all background reads to
    /// `BG_IO_BUSY` concurrent); when playback goes quiet it releases them so
    /// background work speeds back up. Idempotent per process — call once at
    /// startup (needs the tokio runtime). Not spawned in tests (no live
    /// playback), which keeps the semaphore at full `BG_IO_MAX`.
    pub fn spawn_bg_io_regulator(state: Arc<Self>) {
        let sem = state.bg_io.clone();
        actix_web::rt::spawn(async move {
            let reserve = (BG_IO_MAX - BG_IO_BUSY) as u32;
            let mut parked: Option<tokio::sync::OwnedSemaphorePermit> = None;
            loop {
                let active = state.playback_active();
                if active && parked.is_none() {
                    // Grab (and hold) the reserve so only BG_IO_BUSY remain.
                    // acquire_many_owned waits for running bg ops to free
                    // permits, so it converges within ~one op's duration.
                    if let Ok(p) = sem.clone().acquire_many_owned(reserve).await {
                        parked = Some(p);
                        tracing::debug!("bg-io regulator: throttled (playback active)");
                    }
                } else if !active && parked.is_some() {
                    parked = None; // drop → release the reserve
                    tracing::debug!("bg-io regulator: reopened (playback quiet)");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }

    /// Phase B2 — does this replica currently hold background-work
    /// leadership? DB-writing background loops check this before doing work.
    pub fn is_bg_leader(&self) -> bool {
        self.is_bg_leader.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Await background-work leadership — resolves once this replica holds it.
    /// One-shot boot jobs (subtitle / trickplay warm) call this so a follower
    /// runs them only after it has been elected (e.g. the leader was replaced).
    pub async fn wait_until_bg_leader(&self) {
        while !self.is_bg_leader() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// Elect this replica as the background-work singleton, then hold the
    /// leadership lease for the process lifetime. Under Postgres this retries
    /// the advisory lock every 15 s until it wins — so when the current
    /// leader's pod is replaced, a surviving replica takes over within a few
    /// seconds and starts running the background loops. Under SQLite it wins
    /// immediately (single writer). Call once at startup.
    pub fn spawn_bg_leadership(state: Arc<Self>) {
        actix_web::rt::spawn(async move {
            let lease = loop {
                match state.stores.try_acquire_bg_leadership().await {
                    Ok(Some(lease)) => break lease,
                    Ok(None) => {
                        // Another replica leads; retry so we take over if it goes away.
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "bg-leadership election failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    }
                }
            };
            state
                .is_bg_leader
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("acquired background-work leadership");
            // Hold the lease (and, under Postgres, the lock-owning connection)
            // for the process lifetime; dropping it would release leadership.
            let _lease = lease;
            std::future::pending::<()>().await;
        });
    }

    /// T73 — record a dashboard activity entry (newest-first, bounded). `kind`
    /// is the Jellyfin activity type (e.g. `"SessionStarted"`); `overview` is an
    /// optional one-line detail. Best-effort — a poisoned lock is ignored rather
    /// than propagated into a request path.
    pub fn record_activity(
        &self,
        name: &str,
        kind: &str,
        user_id: Option<&str>,
        overview: Option<&str>,
    ) {
        let id = self
            .next_activity_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": kind,
            "Date": crate::api::jellyfin::dto::format_iso8601(unix_now_secs()),
            "UserId": user_id,
            "Severity": "Information",
            "ShortOverview": overview,
        });
        if let Ok(mut log) = self.activity_log.lock() {
            log.push_front(entry);
            while log.len() > ACTIVITY_LOG_CAP {
                log.pop_back();
            }
        }
    }

    /// T73 — a `(total, page)` snapshot of the activity log for
    /// `/System/ActivityLog/Entries`, applying `start_index` + `limit` over the
    /// newest-first buffer.
    pub fn activity_entries(
        &self,
        start_index: usize,
        limit: usize,
    ) -> (usize, Vec<serde_json::Value>) {
        match self.activity_log.lock() {
            Ok(log) => {
                let total = log.len();
                let page = log.iter().skip(start_index).take(limit).cloned().collect();
                (total, page)
            }
            Err(_) => (0, Vec::new()),
        }
    }

    /// Seconds since the last live-segment request (saturating; large when
    /// nobody has streamed recently). The backfill uses this as its
    /// yield-to-playback gate.
    pub fn seconds_since_playback(&self) -> i64 {
        unix_now_secs()
            .saturating_sub(
                self.playback_activity
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .max(0)
    }

    /// Construct with a fresh random `server_id`. Reserved for tests that
    /// don't care about identity persistence — production callers should
    /// use [`AppState::load`] so jellyfin clients don't re-pair across
    /// restarts (T35).
    pub fn new(stores: Stores, server_name: String) -> Self {
        let auth = BuiltinAuth::new(stores.clone());
        let sessions = SessionRegistry::spawn();
        let transcode_sessions = TranscodeSessionRegistry::spawn(stores.clone());
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
            segment_opts_hints: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            played_threshold_pct: 90,
            linux_firefox_h264: false,
            scan_rate_limit_ms: 0,
            scan_probe_concurrency: 0,
            ffmpeg: default_ffmpeg_backend(),
            synth_image_ids: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            synth_image_warm: Arc::new(tokio::sync::Mutex::new(())),
            playback_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            bg_io: Arc::new(tokio::sync::Semaphore::new(BG_IO_MAX)),
            is_bg_leader: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            library_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            item_list_cache: Arc::new(tokio::sync::Mutex::new(None)),
            activity_log: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_activity_id: Arc::new(std::sync::atomic::AtomicI64::new(1)),
            waveform: Default::default(),
            parental_ratings: crate::parental::ParentalRatingMap::us_default(),
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
        let transcode_sessions = TranscodeSessionRegistry::spawn(stores.clone());
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
            segment_opts_hints: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            played_threshold_pct: 90,
            linux_firefox_h264: false,
            scan_rate_limit_ms: 0,
            scan_probe_concurrency: 0,
            ffmpeg: default_ffmpeg_backend(),
            synth_image_ids: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            synth_image_warm: Arc::new(tokio::sync::Mutex::new(())),
            playback_activity: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            bg_io: Arc::new(tokio::sync::Semaphore::new(BG_IO_MAX)),
            is_bg_leader: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            library_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            item_list_cache: Arc::new(tokio::sync::Mutex::new(None)),
            activity_log: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_activity_id: Arc::new(std::sync::atomic::AtomicI64::new(1)),
            waveform: Default::default(),
            parental_ratings: crate::parental::ParentalRatingMap::us_default(),
        })
    }

    /// P36 builder — apply the configured played-threshold,
    /// clamping to `[50, 100]` so a misconfigured 0 doesn't
    /// flip every play to played=true and a 250 doesn't make
    /// played unreachable.
    pub fn with_linux_firefox_h264(mut self, on: bool) -> Self {
        self.linux_firefox_h264 = on;
        self
    }

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

    /// #11 builder — probe fan-out cap for background refresh spawns. 0 keeps
    /// the scanner default; capped at the scanner's ceiling.
    pub fn with_scan_probe_concurrency(mut self, degree: usize) -> Self {
        self.scan_probe_concurrency = degree.min(8);
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

    /// The configured trickplay widths that ACTUALLY have tiles on disk for
    /// `media_id` (B35). Empty when trickplay is disabled or nothing is
    /// generated yet — `with_trickplay` then omits the DTO field, so clients
    /// never render an empty scrub-preview box for tiles that 404.
    pub fn generated_trickplay_widths(&self, media_id: u64) -> Vec<u32> {
        match &self.trickplay {
            Some(cache) => cache.generated_widths(media_id, &self.trickplay_widths),
            None => Vec::new(),
        }
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
        self.library_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.bus.send(SocketBroadcast::LibraryChanged {
            added: Vec::new(),
            removed: Vec::new(),
        });
    }

    /// Full `media_items` list, cached until the library changes (see
    /// `item_list_cache`). Callers that only read/filter the list (home-screen
    /// rows, synth Series/Season resolution) should use this instead of
    /// `stores.list()` to avoid a full-table scan on every request. Returns an
    /// `Arc` so the large Vec is shared, not cloned, across concurrent readers.
    pub async fn list_items_cached(
        &self,
    ) -> Result<Arc<Vec<pharos_core::MediaItem>>, pharos_core::DomainError> {
        use pharos_core::MediaStore;
        /// Backstop: even if some mutation path forgets to bump the generation,
        /// the cache refreshes at least this often.
        const LIST_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);
        let gen = self
            .library_generation
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut guard = self.item_list_cache.lock().await;
        if let Some((cached_gen, at, items)) = guard.as_ref() {
            if *cached_gen == gen && at.elapsed() < LIST_CACHE_MAX_AGE {
                return Ok(items.clone());
            }
        }
        // Miss (stale generation / expired / cold). The mutex makes this
        // single-flight: a burst of home-screen requests shares one scan.
        let items = Arc::new(self.stores.list().await?);
        *guard = Some((gen, std::time::Instant::now(), items.clone()));
        Ok(items)
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
        self.library_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = self.bus.send(SocketBroadcast::LibraryChanged {
            added: added.iter().map(|id| id.to_string()).collect(),
            removed: removed.iter().map(|id| id.to_string()).collect(),
        });
    }

    /// T87 — record the transcode options a play session last used for a
    /// segment, for SyncPlay seek prewarming.
    pub fn note_segment_opts(&self, play_session_id: &str, media_id: u64, opts: &SegmentOpts) {
        if let Ok(mut m) = self.segment_opts_hints.lock() {
            // Sanity cap: play sessions number in the tens; a runaway caller
            // must not grow this unbounded.
            if m.len() > 512 {
                m.clear();
            }
            m.insert(play_session_id.to_string(), (media_id, opts.clone()));
        }
    }

    /// T87 — every recorded (play-session, options) pair currently pointing
    /// at `media_id`.
    pub fn segment_opts_for_media(&self, media_id: u64) -> Vec<SegmentOpts> {
        self.segment_opts_hints
            .lock()
            .map(|m| {
                m.values()
                    .filter(|(mid, _)| *mid == media_id)
                    .map(|(_, o)| o.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fire a `UserDataChanged` event scoped to one user, carrying the
    /// full serialized `UserItemDataDto` for each changed item (B36).
    /// jellyfin-web needs `ItemId` (32-hex), `Key`, `Played`,
    /// `IsFavorite`, `PlayedPercentage` … to patch its UI in place.
    pub fn notify_user_data_changed(&self, user_id: &str, entries: Vec<serde_json::Value>) {
        let _ = self.bus.send(SocketBroadcast::UserDataChanged {
            user_id: user_id.to_string(),
            entries,
        });
    }

    /// Fire a `SessionCommand` event for one target session.
    /// Receivers ignore commands not addressed to them.
    pub fn notify_session_command(
        &self,
        session_id: &str,
        controlling_user_id: &str,
        command: &str,
        arg: serde_json::Value,
    ) {
        let _ = self.bus.send(SocketBroadcast::SessionCommand {
            session_id: session_id.to_string(),
            controlling_user_id: controlling_user_id.to_string(),
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
        item_kind: Option<pharos_core::MediaKind>,
        position_ticks: u64,
        is_paused: bool,
    ) {
        let _ = self.bus.send(SocketBroadcast::PlaybackProgress {
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            item_kind,
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

/// Process-wide "we are draining" flag (B26). Set on SIGTERM/SIGINT (the
/// rolling-deploy drain) so teardown paths can tell "the CLIENT went away"
/// (dismantle its membership) from "WE are going away" (leave every
/// membership + the persisted group snapshot intact for the next process to
/// recover). Without this, the graceful drain closed every /socket, the
/// reconnect-grace teardown removed every member, the emptied group deleted
/// its own snapshot — and B24's restart recovery had nothing to recover.
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark the process as draining. Called from the signal listener in main.
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// True once SIGTERM/SIGINT has been observed.
pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
}
