//! pharos-ui's `NativeAudio` and `OfflineStore` over `pharos_android`: the
//! UI's music plays on the same player the car drives, and its offline
//! screens read the same cache.
//!
//! Both reach the player and library the media service sets up. Android
//! stops that service once the app has been idle in the background, so every
//! music command starts it again ([`pharos_android::start_service`]) and
//! waits briefly for it; a command it never reaches is dropped and logged.

use std::sync::{Arc, OnceLock};

use pharos_android::snapshot::{self, PlayState as HostState, Snapshot};
use pharos_android::{content, Player};
use pharos_ui::native::kept_video_disk::Budget;
use pharos_ui::native::ports::{
    self, ContainerKind, ContainerRef, Download, NativeAudio, OfflineError, OfflineStore,
    PlayState, PlayerSnapshot, TrackDownload, TrackState, VisFrame,
};
use pharos_vis::engine as vis;
use pharos_vis::source;
use tokio::sync::watch;

/// Registers both ports; call before `launch`, so the first render sees them.
pub fn register() {
    for (port, outcome) in [
        (
            "native_audio",
            ports::register_native_audio(Arc::new(PhoneAudio)),
        ),
        (
            "offline_store",
            ports::register_offline_store(Arc::new(PhoneOffline)),
        ),
    ] {
        match outcome {
            Ok(()) => log::info!("pharos: port {port} verdict=registered"),
            Err(_) => log::warn!("pharos: port {port} verdict=already_registered"),
        }
    }
}

struct PhoneAudio;

/// How long a music command waits for the service to start the player.
const PLAYER_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl PhoneAudio {
    fn with(&self, command: &str, f: impl FnOnce(&Player)) {
        crate::activity::start_media_service();
        match pharos_android::player_within(PLAYER_WAIT) {
            Some(p) => f(p),
            None => log::warn!(
                "pharos: native audio command={command} verdict=dropped reason=service_not_started waited_ms={}",
                PLAYER_WAIT.as_millis()
            ),
        }
    }
}

impl NativeAudio for PhoneAudio {
    fn play(&self, ids: Vec<String>, start_index: usize) {
        crate::activity::ask_notifications_on_first_play();
        self.with("play", |p| p.play(ids, start_index));
    }

    fn play_from_start(&self, ids: Vec<String>, start_index: usize) {
        crate::activity::ask_notifications_on_first_play();
        self.with("play_from_start", |p| p.play_from_start(ids, start_index));
    }

    fn set_queue(&self, ids: Vec<String>, current_index: usize) {
        self.with("set_queue", |p| p.set_queue(ids, current_index));
    }

    fn pause(&self) {
        self.with("pause", Player::pause);
    }

    fn resume(&self) {
        self.with("resume", Player::resume);
    }

    fn seek(&self, position_ms: u64) {
        self.with("seek", |p| p.seek(position_ms));
    }

    fn next(&self) {
        self.with("next", Player::next);
    }

    fn previous(&self) {
        self.with("previous", Player::previous);
    }

    fn stop(&self) {
        self.with("stop", Player::stop);
    }

    fn set_volume(&self, level: f32) {
        self.with("set_volume", |p| p.set_volume(level));
    }

    fn state(&self) -> watch::Receiver<PlayerSnapshot> {
        bridge().subscribe()
    }

    fn visualiser_enable(&self, on: bool) {
        analyser().set_enabled(on);
    }

    fn visualiser_frame(&self) -> Option<VisFrame> {
        let pulled = analyser().frame()?;
        Some(VisFrame {
            sample_rate: pulled.sample_rate,
            bins: pulled.frame.bins,
            wave: pulled.frame.wave,
        })
    }
}

/// The visualiser's decoder (ADR-0133). Idle, with no thread work and no
/// memory beyond the engine itself, until the UI shows the visualiser.
fn analyser() -> &'static vis::Engine {
    static ENGINE: OnceLock<vis::Engine> = OnceLock::new();
    ENGINE.get_or_init(|| vis::Engine::new(Arc::new(SongSource)))
}

/// Finds the song for the decoder: the cached file when it is whole, else a
/// ranged read of the stream URL.
struct SongSource;

impl vis::Opener for SongSource {
    fn open(&self, ui_id: &str) -> Result<vis::Opened, String> {
        // The snapshot carries the UI's id; the library knows the node.
        let id = snapshot::car_id(ui_id);
        let id = id.as_str();
        let lib = pharos_android::library().ok_or("the library is not connected")?;
        if let Some(path) = lib.local_path(id) {
            let file = std::fs::File::open(&path).map_err(|e| format!("cached file: {e}"))?;
            return Ok(vis::Opened {
                source: Box::new(file),
                extension: path.extension().and_then(|e| e.to_str()).map(str::to_owned),
            });
        }
        let node = lib.node(id).map_err(|e| e.redacted())?;
        let url = node.stream_url.ok_or("the track has no stream url")?;
        let reader = content::ranged::RangedReader::open(&url).map_err(|e| e.to_string())?;
        let len = reader.len();
        Ok(vis::Opened {
            source: Box::new(source::ReadSeekSource::new(reader, len)),
            extension: None,
        })
    }
}

/// The UI's snapshot channel, fed from the host's by one thread.
fn bridge() -> &'static watch::Sender<PlayerSnapshot> {
    static TX: OnceLock<watch::Sender<PlayerSnapshot>> = OnceLock::new();
    TX.get_or_init(|| {
        let mut host = snapshot::subscribe();
        let tx = watch::Sender::new(to_ui(&host.borrow_and_update()));
        let feed = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("pharos-snapshot".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("pharos: snapshot bridge verdict=stopped reason=runtime:{e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    while host.changed().await.is_ok() {
                        let latest = host.borrow_and_update().clone();
                        analyser().update(
                            latest.state == HostState::Playing,
                            latest.current_id.as_deref(),
                            latest.position_ms,
                        );
                        feed.send_replace(to_ui(&latest));
                    }
                    log::warn!("pharos: snapshot bridge verdict=stopped reason=host_closed");
                });
            });
        if let Err(e) = spawned {
            log::error!("pharos: snapshot bridge verdict=not_started reason={e}");
        }
        tx
    })
}

fn to_ui(s: &Snapshot) -> PlayerSnapshot {
    PlayerSnapshot {
        state: match s.state {
            HostState::Idle => PlayState::Idle,
            HostState::Buffering => PlayState::Buffering,
            HostState::Playing => PlayState::Playing,
            HostState::Paused => PlayState::Paused,
            HostState::Ended => PlayState::Ended,
        },
        position_ms: s.position_ms,
        duration_ms: s.duration_ms,
        current_id: s.current_id.clone(),
        queue: s.queue.clone(),
        index: s.index,
        error: s.error.clone(),
    }
}

struct PhoneOffline;

fn library(op: &str) -> Result<&'static content::Library, OfflineError> {
    pharos_android::library()
        .ok_or_else(|| OfflineError::Host(format!("{op}: the library is not connected")))
}

/// The library's id for a UI container: `album/<item id>`, or `mix/<n>` for
/// the Nth built-in mix, which the UI names by its title. `None` for a mix
/// the library has no id for (a saved one).
fn container_id(c: &ContainerRef) -> Option<String> {
    match c.kind {
        ContainerKind::Album => Some(format!("album/{}", c.id)),
        ContainerKind::Mix => pharos_client::mix::presets()
            .iter()
            .position(|m| m.name == c.id)
            .map(|n| format!("mix/{n}")),
    }
}

/// Downloaded video shares the music cache's budget: one cap for what the
/// phone keeps, set where the music cache's is, with the music's bytes
/// counted against it. Before the media service has started there is no
/// cache to ask, so the default cap stands and the music counts as nothing.
struct PhoneBudget {
    dir: std::path::PathBuf,
}

impl Budget for PhoneBudget {
    fn cap_bytes(&self) -> u64 {
        pharos_android::library()
            .and_then(content::Library::cache_cap)
            .unwrap_or(content::DEFAULT_CAP_BYTES)
    }

    fn other_used_bytes(&self) -> u64 {
        pharos_android::library()
            .and_then(content::Library::offline_summary)
            .map_or(0, |s| s.used_bytes)
    }

    fn admission_guard(&self) -> Box<dyn std::any::Any> {
        // The lock the music cache holds while it adds a track and trims,
        // so neither kind admits against a figure the other is changing.
        Box::new(content::admission_guard())
    }

    fn free_bytes(&self) -> u64 {
        match content::statvfs_free(&self.dir) {
            Ok(free) => free,
            Err(e) => {
                // Unknown room is no room: a download is refused, not risked.
                log::warn!(
                    "pharos: video free space unreadable dir={} error={e}",
                    self.dir.display()
                );
                0
            }
        }
    }
}

/// Opens the video store under the app's files directory and registers it.
/// Run on the first render of the root component: the files directory comes
/// from the activity, which is there by then, and the first screen then
/// sees the store.
pub fn register_video() {
    let root = match crate::activity::files_dir() {
        Ok(dir) => dir.join("video"),
        Err(e) => {
            log::warn!(
                "pharos: port video_store verdict=not_registered reason=no_files_dir error={e}"
            );
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&root) {
        log::warn!(
            "pharos: port video_store verdict=not_registered reason=no_folder dir={} error={e}",
            root.display()
        );
        return;
    }
    // The music cache counts the video's bytes against the shared cap.
    pharos_android::set_outside_used(pharos_ui::native::ports::outside_bytes_hook());
    let budget = Arc::new(PhoneBudget { dir: root.clone() });
    match pharos_ui::native::register_phone_video_store(root.clone(), budget) {
        Ok(()) => log::info!(
            "pharos: port video_store verdict=registered dir={}",
            root.display()
        ),
        Err(e) => log::warn!("pharos: port video_store verdict=not_registered reason={e}"),
    }
}

/// The UI's container for a library id; `None` for anything but an album or
/// a built-in mix.
fn container_ref(id: &str) -> Option<ContainerRef> {
    let (kind, rest) = id.split_once('/')?;
    match kind {
        "album" => Some(ContainerRef {
            kind: ContainerKind::Album,
            id: rest.to_owned(),
        }),
        "mix" => {
            let n: usize = rest.parse().ok()?;
            let name = pharos_client::mix::presets().into_iter().nth(n)?.name;
            Some(ContainerRef {
                kind: ContainerKind::Mix,
                id: name,
            })
        }
        _ => None,
    }
}

/// [`container_id`], or the error the UI shows for a container the library
/// cannot keep.
fn library_id(c: &ContainerRef) -> Result<String, OfflineError> {
    container_id(c).ok_or_else(|| {
        log::warn!(
            "pharos: offline container verdict=unknown reason=no_library_id kind={}",
            c.kind.label()
        );
        OfflineError::UnknownContainer {
            kind: c.kind,
            id: c.id.clone(),
        }
    })
}

fn to_ui_download(d: content::Download) -> Option<Download> {
    let Some(container) = container_ref(&d.container) else {
        log::warn!(
            "pharos: offline download verdict=skipped reason=not_a_container id={}",
            d.container
        );
        return None;
    };
    let subtitle = match container.kind {
        ContainerKind::Album => d.subtitle,
        _ => None,
    };
    Some(Download {
        container,
        title: d.title.unwrap_or_default(),
        subtitle,
        tracks: d
            .tracks
            .into_iter()
            .map(|t| TrackDownload {
                id: snapshot::ui_id(&t.id),
                title: t.title.unwrap_or_default(),
                state: track_state(t.complete, t.failed),
                bytes: t.bytes,
            })
            .collect(),
    })
}

fn track_state(complete: bool, failed: bool) -> TrackState {
    match (complete, failed) {
        (true, _) => TrackState::Cached,
        (false, true) => TrackState::Failed,
        (false, false) => TrackState::Queued,
    }
}

fn pin_now(
    lib: &content::Library,
    id: &str,
    kind: ContainerKind,
    ui_id: &str,
) -> Result<(), OfflineError> {
    match lib.pin(id) {
        Ok(tracks) => {
            log::info!("pharos: offline pin id={id} tracks={tracks} verdict=pinned");
            Ok(())
        }
        Err(content::ContentError::BadId(_)) => {
            log::warn!("pharos: offline pin id={id} verdict=failed reason=bad_id");
            Err(OfflineError::UnknownContainer {
                kind,
                id: ui_id.to_owned(),
            })
        }
        Err(e) => {
            let why = e.redacted();
            log::warn!("pharos: offline pin id={id} verdict=failed reason={why}");
            Err(OfflineError::Host(why))
        }
    }
}

impl OfflineStore for PhoneOffline {
    fn cache_cap_bytes(&self) -> u64 {
        pharos_android::library()
            .and_then(content::Library::cache_cap)
            .unwrap_or(content::DEFAULT_CAP_BYTES)
    }

    fn set_cache_cap_bytes(&self, bytes: u64) -> Result<(), OfflineError> {
        let lib = library("set cap")?;
        lib.set_cache_cap(bytes)
            .map_err(|e| OfflineError::Host(e.redacted()))?;
        log::info!("pharos: offline cap_bytes={bytes} verdict=set");
        Ok(())
    }

    fn used_bytes(&self) -> u64 {
        pharos_android::library()
            .and_then(content::Library::offline_summary)
            .map_or(0, |s| s.used_bytes)
    }

    fn free_bytes(&self) -> u64 {
        pharos_android::library()
            .and_then(content::Library::offline_summary)
            .and_then(|s| s.free_bytes)
            .unwrap_or(0)
    }

    fn pin(&self, container: &ContainerRef) -> Result<(), OfflineError> {
        let id = library_id(container)?;
        let lib = library("pin")?;
        // Online, pinning fetches the listing from the server, and the Keep
        // toggle calls this from its click handler: run it on a worker. The
        // toggle polls the downloads, so the pin shows when it lands.
        // Offline it only reads the saved listing, so it stays in line and
        // its error reaches the toggle.
        if lib.is_online() {
            let kind = container.kind;
            let ui_id = container.id.clone();
            let spawned = std::thread::Builder::new()
                .name("pharos-pin".into())
                .spawn(move || {
                    let _ = pin_now(lib, &id, kind, &ui_id);
                });
            return match spawned {
                Ok(_) => Ok(()),
                Err(e) => Err(OfflineError::Host(format!("pin worker: {e}"))),
            };
        }
        pin_now(lib, &id, container.kind, &container.id)
    }

    fn unpin(&self, container: &ContainerRef) -> Result<(), OfflineError> {
        let id = library_id(container)?;
        library("unpin")?.unpin(&id);
        log::info!("pharos: offline unpin id={id} verdict=unpinned");
        Ok(())
    }

    fn downloads(&self) -> Vec<Download> {
        pharos_android::library()
            .map(content::Library::downloads)
            .unwrap_or_default()
            .into_iter()
            .filter_map(to_ui_download)
            .collect()
    }

    fn is_cached(&self, track_id: &str) -> bool {
        pharos_android::library()
            .is_some_and(|lib| lib.local_path(&snapshot::car_id(track_id)).is_some())
    }

    fn is_online(&self) -> bool {
        pharos_android::library().is_some_and(content::Library::is_online)
    }

    fn saved_position_ms(&self, track_id: &str) -> Option<u64> {
        pharos_android::saved_resume_ms(&snapshot::car_id(track_id))
    }
}
