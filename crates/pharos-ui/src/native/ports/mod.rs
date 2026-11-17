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
static VIDEO_STORE: Slot<dyn super::kept_video::VideoStore> = Slot::new();

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

/// Register the store of downloaded videos (spec 048): the web app's Cache
/// Storage store, or the phone's. The Download control and the Downloads
/// page draw only when one is registered.
pub fn register_video_store(
    port: Arc<dyn super::kept_video::VideoStore>,
) -> Result<(), AlreadyRegistered> {
    VIDEO_STORE.set(port)
}

/// The registered store of downloaded videos, if any.
pub fn video_store() -> Option<Arc<dyn super::kept_video::VideoStore>> {
    VIDEO_STORE.get()
}

/// Bytes the registered video store holds or has reserved, for every user;
/// 0 when none is registered. The phone counts them against the cap it
/// shares with the music cache.
pub fn kept_video_bytes() -> u64 {
    video_store().map_or(0, |s| s.kept_bytes())
}

/// Bytes the registered video store is admitted for but has yet to write,
/// for every user; 0 when none is registered. The phone takes them off the
/// free space before the music cache's low-disk guard.
pub fn kept_video_outstanding() -> u64 {
    video_store().map_or(0, |s| s.outstanding_bytes())
}

/// Whether the registered video store holds a folder of unknown size, in
/// which case the music cache admits nothing either; false when none is
/// registered.
pub fn kept_video_size_unknown() -> bool {
    video_store().is_some_and(|s| s.size_unknown())
}

/// The hook the phone registers with the music cache: what the registered
/// video store holds, still owes, and whether it knows its own size. The
/// phone registers exactly this (`pharos-phone` cannot host a test, being
/// Android-only), and the integration tests call it too.
#[cfg(any(feature = "mobile", test))]
pub fn outside_bytes_hook() -> pharos_content::OutsideUsed {
    Arc::new(|| pharos_content::OutsideBytes {
        held: kept_video_bytes(),
        outstanding: kept_video_outstanding(),
        unknown: kept_video_size_unknown(),
    })
}

/// What the host app runs when the user signs in or out, so a second client
/// in the same process (the car's media service) re-reads the token.
static ON_CREDENTIALS_CHANGED: OnceLock<fn()> = OnceLock::new();

/// Called once by the host app before the UI launches.
pub fn register_on_credentials_changed(hook: fn()) -> Result<(), AlreadyRegistered> {
    ON_CREDENTIALS_CHANGED
        .set(hook)
        .map_err(|_| AlreadyRegistered)
}

/// The thread that runs the hook, so the caller (the UI) never waits for the
/// host's reload, which queues behind the content worker. One thread keeps the
/// changes in the order they happened.
static CREDENTIALS_NOTIFIER: OnceLock<std::sync::mpsc::Sender<fn()>> = OnceLock::new();

fn notifier() -> Option<&'static std::sync::mpsc::Sender<fn()>> {
    if let Some(tx) = CREDENTIALS_NOTIFIER.get() {
        return Some(tx);
    }
    let (tx, rx) = std::sync::mpsc::channel::<fn()>();
    std::thread::Builder::new()
        .name("pharos-credentials".into())
        .spawn(move || {
            while let Ok(hook) = rx.recv() {
                hook();
            }
        })
        .ok()?;
    Some(CREDENTIALS_NOTIFIER.get_or_init(|| tx))
}

/// Queues the host's hook to run after the stored token changed, off the
/// caller's thread; returns whether one was registered.
pub fn credentials_changed() -> bool {
    let Some(hook) = ON_CREDENTIALS_CHANGED.get() else {
        return false;
    };
    queue_hook(notifier(), *hook);
    true
}

/// Hands the hook to the notifier thread. It is never run on the caller's
/// thread: the hook can block on the content worker, and the caller is the UI.
/// A missing thread or a closed channel drops this call with a warning.
fn queue_hook(tx: Option<&std::sync::mpsc::Sender<fn()>>, hook: fn()) -> bool {
    match tx {
        Some(tx) => match tx.send(hook) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("warn: credentials hook not queued, notifier thread is gone: {e}");
                false
            }
        },
        None => {
            eprintln!("warn: credentials hook not queued, notifier thread did not start");
            false
        }
    }
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

/// One frame of the visualiser's analysis of the song now playing, made by
/// the host (ADR-0133) in the web analyser's terms and quantised to bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisFrame {
    /// The song's sample rate, which sets what each bin's frequency is.
    pub sample_rate: u32,
    /// Each bin's level: byte `b` is `-b * 0.5` dBFS, 255 for none.
    pub bins: Vec<u8>,
    /// The latest samples, twice as many as `bins`: `s / 127`, -1 to 1.
    pub wave: Vec<i8>,
}

impl VisFrame {
    /// Bins in a frame: half the analyser's FFT size.
    pub const BINS: usize = 1024;

    /// A frame of no sound, which draws as an idle visualiser.
    pub fn silent(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            bins: vec![u8::MAX; Self::BINS],
            wave: vec![0; 2 * Self::BINS],
        }
    }
}

/// The native audio engine. Commands return at once: the host hops to its own
/// worker, and the outcome arrives as a snapshot. The UI never blocks on JNI.
pub trait NativeAudio: Send + Sync {
    /// Replace the queue with `ids` and start at `start_index`.
    fn play(&self, ids: Vec<String>, start_index: usize);
    /// [`NativeAudio::play`] that starts at 0 even when the host has a saved
    /// position for the song (Start over). A host with no saved positions
    /// needs no override.
    fn play_from_start(&self, ids: Vec<String>, start_index: usize) {
        self.play(ids, start_index);
    }
    /// Replace the queue with `ids`, with `current_index` as the song now
    /// playing. The current song keeps playing: no restart, no gap.
    fn set_queue(&self, ids: Vec<String>, current_index: usize);
    fn pause(&self);
    fn resume(&self);
    fn seek(&self, position_ms: u64);
    fn next(&self);
    fn previous(&self);
    fn stop(&self);
    /// The engine's output level, 0.0 (silent) to 1.0 (full). Out-of-range
    /// values are the caller's to clamp.
    fn set_volume(&self, level: f32);
    /// A receiver that always holds the latest snapshot.
    fn state(&self) -> watch::Receiver<PlayerSnapshot>;
    /// The visualiser is shown (`true`) or gone (`false`). The host analyses
    /// the song only while it is shown and playing. A host with no analyser
    /// ignores it.
    fn visualiser_enable(&self, _on: bool) {}
    /// The frame under the playhead now, or `None`: nothing plays, the song
    /// cannot be decoded, or the analysis is not ready.
    fn visualiser_frame(&self) -> Option<VisFrame> {
        None
    }
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
    /// Where the phone last stopped in a long track (a book's part), in ms,
    /// for choosing which part to open. A host that keeps none says `None`.
    fn saved_position_ms(&self, _track_id: &str) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static RAN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn count() {
        RAN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn a_hook_with_no_notifier_is_dropped_not_run_on_the_caller() {
        let before = RAN.load(std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<fn()>();
        assert!(!queue_hook(None, count));
        drop(rx);
        assert!(!queue_hook(Some(&tx), count));
        assert_eq!(RAN.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    #[test]
    fn a_hook_is_queued_not_run_inline() {
        let before = RAN.load(std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<fn()>();
        assert!(queue_hook(Some(&tx), count));
        assert_eq!(RAN.load(std::sync::atomic::Ordering::SeqCst), before);
        assert!(rx.try_recv().is_ok());
    }

    struct Stub(watch::Sender<PlayerSnapshot>);

    impl Stub {
        fn new() -> Self {
            Self(watch::Sender::new(PlayerSnapshot::default()))
        }
    }

    impl NativeAudio for Stub {
        fn play(&self, _: Vec<String>, _: usize) {}
        fn set_queue(&self, _: Vec<String>, _: usize) {}
        fn pause(&self) {}
        fn resume(&self) {}
        fn seek(&self, _: u64) {}
        fn next(&self) {}
        fn previous(&self) {}
        fn stop(&self) {}
        fn set_volume(&self, _: f32) {}
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

    #[test]
    fn a_credentials_change_runs_the_registered_hook_once_per_change() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn hook() {
            std::thread::sleep(std::time::Duration::from_millis(150));
            CALLS.fetch_add(1, Ordering::SeqCst);
        }
        assert!(!credentials_changed());
        assert_eq!(register_on_credentials_changed(hook), Ok(()));
        assert_eq!(
            register_on_credentials_changed(hook),
            Err(AlreadyRegistered)
        );
        let asked = std::time::Instant::now();
        assert!(credentials_changed());
        assert!(credentials_changed());
        assert!(
            asked.elapsed() < std::time::Duration::from_millis(100),
            "the caller does not wait for the hook"
        );
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while CALLS.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }
}
