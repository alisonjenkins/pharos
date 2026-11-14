//! Ports to the phone's native player and offline store (spec 044, D4).
//!
//! The UI calls these traits with plain Rust calls; `pharos-phone` implements
//! them over JNI and registers them before launch. Nothing here knows about
//! Android. Where nothing is registered (web, desktop) the features that need
//! them stay hidden.

use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

pub mod fake;

/// A set-once cell holding a port implementation.
pub struct Slot<T: ?Sized> {
    cell: OnceLock<Arc<T>>,
}

impl<T: ?Sized> Slot<T> {
    pub const fn new() -> Self {
        Self {
            cell: OnceLock::new(),
        }
    }

    /// Refuses a second port, so a late registration cannot silently replace
    /// the one the UI already reads.
    pub fn set(&self, port: Arc<T>) -> Result<(), AlreadyRegistered> {
        self.cell.set(port).map_err(|_| AlreadyRegistered)
    }

    pub fn get(&self) -> Option<Arc<T>> {
        self.cell.get().cloned()
    }
}

impl<T: ?Sized> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a port is already registered; registration happens once, before launch")]
pub struct AlreadyRegistered;

static NATIVE_AUDIO: Slot<dyn NativeAudio> = Slot::new();
static OFFLINE_STORE: Slot<dyn OfflineStore> = Slot::new();

/// Called once by the host app before the UI launches.
pub fn register_native_audio(port: Arc<dyn NativeAudio>) -> Result<(), AlreadyRegistered> {
    NATIVE_AUDIO.set(port)
}

/// `None` on web and desktop: the UI hides what needs native audio.
pub fn native_audio() -> Option<Arc<dyn NativeAudio>> {
    NATIVE_AUDIO.get()
}

/// Called once by the host app before the UI launches.
pub fn register_offline_store(port: Arc<dyn OfflineStore>) -> Result<(), AlreadyRegistered> {
    OFFLINE_STORE.set(port)
}

/// `None` on web and desktop: the UI hides offline music.
pub fn offline_store() -> Option<Arc<dyn OfflineStore>> {
    OFFLINE_STORE.get()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Idle,
    Buffering,
    Playing,
    Paused,
    Ended,
}

impl PlayState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Buffering => "buffering",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
        }
    }
}

/// What the player is doing now. Published on every change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerSnapshot {
    pub state: PlayState,
    pub position_ms: u64,
    /// `None` until the engine knows it.
    pub duration_ms: Option<u64>,
    pub current_id: Option<String>,
    pub queue: Vec<String>,
    /// Index of `current_id` in `queue`.
    pub index: Option<usize>,
    /// The cause of the last playback failure, with the offending value.
    pub error: Option<String>,
}

/// The native audio engine. Commands return at once: the host hops to its own
/// worker, and the outcome arrives as a snapshot. The UI never blocks on JNI.
pub trait NativeAudio: Send + Sync {
    /// Replace the queue with `ids` and start at `start_index`.
    fn play(&self, ids: Vec<String>, start_index: usize);
    fn pause(&self);
    fn resume(&self);
    fn seek(&self, position_ms: u64);
    fn next(&self);
    fn previous(&self);
    fn stop(&self);
    /// A receiver that always holds the latest snapshot.
    fn state(&self) -> watch::Receiver<PlayerSnapshot>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerKind {
    Album,
    Mix,
}

impl ContainerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Mix => "mix",
        }
    }
}

/// An album or a mix, the unit a user keeps for offline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerRef {
    pub kind: ContainerKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    Queued,
    Cached,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDownload {
    pub id: String,
    pub title: String,
    pub state: TrackState,
    pub bytes: u64,
}

/// One pinned container and its tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub container: ContainerRef,
    pub title: String,
    /// The artist for an album; `None` for a mix.
    pub subtitle: Option<String>,
    pub tracks: Vec<TrackDownload>,
}

impl Download {
    /// Bytes of the tracks already cached.
    pub fn cached_bytes(&self) -> u64 {
        self.tracks
            .iter()
            .filter(|t| t.state == TrackState::Cached)
            .fold(0u64, |sum, t| sum.saturating_add(t.bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfflineError {
    #[error("unknown {} {id:?}", kind.label())]
    UnknownContainer { kind: ContainerKind, id: String },
    #[error("pinning needs {needed} bytes but the cap leaves {available}")]
    OverCap { needed: u64, available: u64 },
    #[error("offline store failed: {0}")]
    Host(String),
}

/// The offline cache and its pins.
pub trait OfflineStore: Send + Sync {
    fn cache_cap_bytes(&self) -> u64;
    fn set_cache_cap_bytes(&self, bytes: u64) -> Result<(), OfflineError>;
    fn used_bytes(&self) -> u64;
    fn free_bytes(&self) -> u64;
    fn pin(&self, container: &ContainerRef) -> Result<(), OfflineError>;
    fn unpin(&self, container: &ContainerRef) -> Result<(), OfflineError>;
    fn downloads(&self) -> Vec<Download>;
    fn is_cached(&self, track_id: &str) -> bool;
    /// Whether the phone can reach the server now. Offline, only cached
    /// tracks play.
    fn is_online(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(watch::Sender<PlayerSnapshot>);

    impl Stub {
        fn new() -> Self {
            Self(watch::Sender::new(PlayerSnapshot::default()))
        }
    }

    impl NativeAudio for Stub {
        fn play(&self, _: Vec<String>, _: usize) {}
        fn pause(&self) {}
        fn resume(&self) {}
        fn seek(&self, _: u64) {}
        fn next(&self) {}
        fn previous(&self) {}
        fn stop(&self) {}
        fn state(&self) -> watch::Receiver<PlayerSnapshot> {
            self.0.subscribe()
        }
    }

    /// Registration happens once; a second port must not replace the first.
    #[test]
    fn a_slot_is_empty_until_set_and_refuses_a_second_port() {
        let slot: Slot<dyn NativeAudio> = Slot::new();
        assert!(slot.get().is_none());
        let first: Arc<dyn NativeAudio> = Arc::new(Stub::new());
        assert_eq!(slot.set(first.clone()), Ok(()));
        let second: Arc<dyn NativeAudio> = Arc::new(Stub::new());
        assert_eq!(slot.set(second), Err(AlreadyRegistered));
        let got = slot.get();
        assert!(got.is_some_and(|g| Arc::ptr_eq(&g, &first)));
    }

    #[test]
    fn the_global_slots_are_empty_without_a_host() {
        assert!(native_audio().is_none());
        assert!(offline_store().is_none());
    }

    #[test]
    fn play_state_labels_are_distinct() {
        let all = [
            PlayState::Idle,
            PlayState::Buffering,
            PlayState::Playing,
            PlayState::Paused,
            PlayState::Ended,
        ];
        let mut labels: Vec<_> = all.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn cached_bytes_counts_only_cached_tracks() {
        let track = |id: &str, state, bytes| TrackDownload {
            id: id.into(),
            title: format!("Track {id}"),
            state,
            bytes,
        };
        let d = Download {
            container: ContainerRef {
                kind: ContainerKind::Album,
                id: "a".into(),
            },
            title: "Album One".into(),
            subtitle: Some("Artist One".into()),
            tracks: vec![
                track("1", TrackState::Cached, 10),
                track("2", TrackState::Queued, 20),
                track("3", TrackState::Failed, 40),
                track("4", TrackState::Cached, 5),
            ],
        };
        assert_eq!(d.cached_bytes(), 15);
    }
}
