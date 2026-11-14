//! pharos-ui's `NativeAudio` and `OfflineStore` over `pharos_android`: the
//! UI's music plays on the same player the car drives, and its offline
//! screens read the same cache.
//!
//! Both reach the player and library the media service sets up, so the app
//! starts that service first ([`pharos_android::start_service`]). A command
//! sent before it runs is dropped and logged.

use std::sync::{Arc, OnceLock};

use pharos_android::snapshot::{self, PlayState as HostState, Snapshot};
use pharos_android::{content, Player};
use pharos_ui::native::ports::{
    self, ContainerKind, ContainerRef, Download, NativeAudio, OfflineError, OfflineStore,
    PlayState, PlayerSnapshot, TrackDownload, TrackState,
};
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

impl PhoneAudio {
    fn with(&self, command: &str, f: impl FnOnce(&Player)) {
        match pharos_android::player() {
            Some(p) => f(p),
            None => log::warn!(
                "pharos: native audio command={command} verdict=dropped reason=service_not_started"
            ),
        }
    }
}

impl NativeAudio for PhoneAudio {
    fn play(&self, ids: Vec<String>, start_index: usize) {
        crate::activity::ask_notifications_on_first_play();
        self.with("play", |p| p.play(ids, start_index));
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
                        feed.send_replace(to_ui(&host.borrow_and_update()));
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

/// The library's id for a UI container: `album/<item id>` or `mix/<n>`.
fn container_id(c: &ContainerRef) -> String {
    format!("{}/{}", c.kind.label(), c.id)
}

/// The UI's container for a library id; `None` for anything but an album or
/// a mix.
fn container_ref(id: &str) -> Option<ContainerRef> {
    let (kind, rest) = id.split_once('/')?;
    let kind = match kind {
        "album" => ContainerKind::Album,
        "mix" => ContainerKind::Mix,
        _ => return None,
    };
    Some(ContainerRef {
        kind,
        id: rest.to_owned(),
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
                state: if t.complete {
                    TrackState::Cached
                } else {
                    TrackState::Queued
                },
                bytes: t.bytes,
            })
            .collect(),
    })
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
        let id = container_id(container);
        let lib = library("pin")?;
        match lib.pin(&id) {
            Ok(tracks) => {
                log::info!("pharos: offline pin id={id} tracks={tracks} verdict=pinned");
                Ok(())
            }
            Err(content::ContentError::BadId(_)) => Err(OfflineError::UnknownContainer {
                kind: container.kind,
                id: container.id.clone(),
            }),
            Err(e) => {
                let why = e.redacted();
                log::warn!("pharos: offline pin id={id} verdict=failed reason={why}");
                Err(OfflineError::Host(why))
            }
        }
    }

    fn unpin(&self, container: &ContainerRef) -> Result<(), OfflineError> {
        let id = container_id(container);
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
}
