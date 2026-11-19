//! Ports to the phone's native player and offline store (spec 044, D4).
//!
//! The UI calls these traits with plain Rust calls; `pharos-phone` implements
//! them over JNI and registers them before launch. Nothing here knows about
//! Android. Where nothing is registered (web, desktop) the features that need
//! them stay hidden.

use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

pub mod fake;

/// A set-once cell holding a port implementation. A port set after a
/// reader looked is announced: [`Slot::published`] resolves then.
pub struct Slot<T: ?Sized> {
    cell: OnceLock<Arc<T>>,
    /// True once the cell is set; made on first use.
    published: OnceLock<watch::Sender<bool>>,
}

impl<T: ?Sized> Slot<T> {
    pub const fn new() -> Self {
        Self {
            cell: OnceLock::new(),
            published: OnceLock::new(),
        }
    }

    fn notifier(&self) -> &watch::Sender<bool> {
        self.published.get_or_init(|| watch::channel(false).0)
    }

    /// Refuses a second port, so a late registration cannot silently replace
    /// the one the UI already reads.
    pub fn set(&self, port: Arc<T>) -> Result<(), AlreadyRegistered> {
        self.cell.set(port).map_err(|_| AlreadyRegistered)?;
        self.notifier().send_replace(true);
        Ok(())
    }

    /// The port, once one is set: now if it is, else when it is. Never
    /// blocks a thread; a reader that must not wait reads [`Slot::get`].
    pub async fn published(&self) -> Arc<T> {
        // Subscribed before the cell is read, so a set in between is seen.
        let mut rx = self.notifier().subscribe();
        loop {
            if let Some(port) = self.get() {
                return port;
            }
            if rx.changed().await.is_err() {
                // The sender lives in the slot: it is never dropped.
                std::future::pending::<()>().await;
            }
        }
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
    video_slot().set(port)?;
    // A sign-in that landed before the store resumes its downloads now.
    super::offline_video::store_published();
    Ok(())
}

/// The registered store of downloaded videos, if any. A component reads it
/// through `kept_video_view::shown_video_store`, which re-renders it when a
/// store registered after it mounted is published.
pub fn video_store() -> Option<Arc<dyn super::kept_video::VideoStore>> {
    video_slot().get()
}

/// The slot behind [`video_store`], for a reader that waits for it.
pub fn video_slot() -> &'static Slot<dyn super::kept_video::VideoStore> {
    #[cfg(test)]
    if let Some(slot) = TEST_VIDEO_SLOT.with(std::cell::Cell::get) {
        return slot;
    }
    &VIDEO_STORE
}

#[cfg(test)]
thread_local! {
    /// A test's own video slot, on this thread, in place of the process's:
    /// a test can publish a store without reaching every other test.
    static TEST_VIDEO_SLOT: std::cell::Cell<Option<&'static Slot<dyn super::kept_video::VideoStore>>> =
        const { std::cell::Cell::new(None) };
}

/// Puts `slot` in place of the process's video slot on this thread, or
/// takes it out with `None`.
#[cfg(test)]
pub fn use_test_video_slot(slot: Option<&'static Slot<dyn super::kept_video::VideoStore>>) {
    TEST_VIDEO_SLOT.with(|s| s.set(slot));
}

/// A claim that succeeds once. Opening the video store twice on one root
/// is not harmless: the second open's recovery deletes the first's staging
/// files. The phone has two callers (the media service's init hook and the
/// first render), so each claims before it touches the folder; a first
/// attempt that reached the store is not retried by the second, which would
/// open the store over whatever the first left. One that failed before it
/// (the folder could not be made) gives the claim back, so a later caller
/// tries again (see [`register_video_root`]).
#[cfg(any(feature = "mobile", test))]
pub struct OpenOnce {
    claimed: std::sync::atomic::AtomicBool,
    /// The music hook is in; it goes in once, whoever claims.
    hooked: std::sync::atomic::AtomicBool,
}

#[cfg(any(feature = "mobile", test))]
impl OpenOnce {
    pub const fn new() -> Self {
        Self {
            claimed: std::sync::atomic::AtomicBool::new(false),
            hooked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// True for the first caller only, until the claim is given back.
    pub fn claim(&self) -> bool {
        !self.claimed.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Gives the claim back: only for an attempt that never reached the
    /// store, so no store can be opening or open on the root.
    fn release(&self) {
        self.claimed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// True the first time only: whether to install the music hook.
    fn first_hook(&self) -> bool {
        !self.hooked.swap(true, std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(any(feature = "mobile", test))]
impl Default for OpenOnce {
    fn default() -> Self {
        Self::new()
    }
}

/// What the video budget reads of the music side. The phone's `Budget`
/// takes every figure from the library through these functions, so the
/// decision is the same code in a test as on the device: the library
/// implements this by delegating to its own methods.
#[cfg(any(feature = "mobile", test))]
pub trait MusicSide {
    /// The cap in force, when the music cache is enabled.
    fn cache_cap(&self) -> Option<u64>;
    /// Bytes the music holds, when the cache is enabled.
    fn used_bytes(&self) -> Option<u64>;
    /// Whether the music cache is enabled.
    fn cache_enabled(&self) -> bool;
    /// Music writes approved and not yet landed.
    fn outstanding_bytes(&self) -> u64;
    /// Bytes music's in-flight downloads have already written: in the cap,
    /// not in the free-space figure. None when they could not all be read.
    fn in_progress_bytes(&self) -> Option<u64>;
}

#[cfg(any(feature = "mobile", test))]
impl MusicSide for pharos_content::Library {
    fn cache_cap(&self) -> Option<u64> {
        pharos_content::Library::cache_cap(self)
    }
    fn used_bytes(&self) -> Option<u64> {
        self.offline_summary().map(|s| s.used_bytes)
    }
    fn cache_enabled(&self) -> bool {
        pharos_content::Library::cache_enabled(self)
    }
    fn outstanding_bytes(&self) -> u64 {
        self.music_outstanding_bytes()
    }
    fn in_progress_bytes(&self) -> Option<u64> {
        self.music_in_progress_bytes()
    }
}

/// The shared cap: the cache's, or the default while there is no cache.
#[cfg(any(feature = "mobile", test))]
pub fn music_cap(music: Option<&dyn MusicSide>) -> u64 {
    music
        .and_then(MusicSide::cache_cap)
        .unwrap_or(pharos_content::DEFAULT_CAP_BYTES)
}

/// What the music holds of the cap; nothing while there is no cache.
#[cfg(any(feature = "mobile", test))]
pub fn music_used(music: Option<&dyn MusicSide>) -> u64 {
    music.and_then(MusicSide::used_bytes).unwrap_or(0)
}

/// Whether the cap and the music's figures can be had: only once the cache
/// is enabled, the cap is the default and the music counts as nothing.
#[cfg(any(feature = "mobile", test))]
pub fn music_ready(music: Option<&dyn MusicSide>) -> bool {
    music.is_some_and(MusicSide::cache_enabled)
}

/// Music writes approved and not yet landed: room the free space does not
/// lack yet but will.
#[cfg(any(feature = "mobile", test))]
pub fn music_outstanding(music: Option<&dyn MusicSide>) -> u64 {
    music.map_or(0, MusicSide::outstanding_bytes)
}

/// Bytes music's in-flight downloads have written already: part of the cap
/// (with `music_used` and `music_outstanding`), never of free space.
#[cfg(any(feature = "mobile", test))]
pub fn music_in_progress(music: Option<&dyn MusicSide>) -> Option<u64> {
    music.map_or(Some(0), MusicSide::in_progress_bytes)
}

/// How `register_video_root` ended.
#[cfg(any(feature = "mobile", test))]
#[derive(Debug, PartialEq, Eq)]
pub enum Registration {
    /// Another caller had claimed the root; nothing was touched.
    AlreadyOpened,
    /// The folder could not be created: no store, and the music cache has
    /// been told the video size is unknown. The claim was given back, so a
    /// later caller tries again.
    NoFolder(String),
    /// The store was opened and registered.
    Registered,
    /// The store was opened but not registered (one already is).
    NotRegistered(String),
    /// The store's runtime or thread could not start, so no store was
    /// opened on the root. The claim was given back, so a later caller
    /// tries again.
    NotOpened(String),
}

/// Whether a registration attempt is done with, for a caller that retries
/// until it is (the service's init hook): only once a store is published.
/// `AlreadyOpened` with nothing published is another caller still opening
/// the store, which may yet fail before it opens and give the claim back:
/// not done, so the retry goes on.
#[cfg(any(feature = "mobile", test))]
pub fn registration_done(_outcome: &Registration, published: bool) -> bool {
    published
}

/// Why `register_video_root`'s `register` failed: before the store was
/// opened on the root (nothing to protect, so the claim is given back) or
/// after it may have been (the claim is kept: a second store would run
/// recovery over the first's staging files).
#[cfg(any(feature = "mobile", test))]
#[derive(Debug, PartialEq, Eq)]
pub enum RegisterFailed {
    /// Nothing was opened on the root.
    BeforeOpen(String),
    /// The store may be open on the root.
    AfterOpen(String),
}

/// The phone's registration of its video store, as the Android crate runs
/// it (the init hook and the first render both come here). `once` is
/// claimed first, before the folder is touched: a second store on the same
/// root would run recovery over the first's staging files, so an attempt
/// that reached `register` (the store may be opening or open) is never
/// retried. One whose folder could not be created opened nothing and gives
/// the claim back: a later caller tries the folder again. `install_hook`
/// goes in once, before the folder is created, and reports the size unknown
/// while no store is registered, so a folder that cannot be created leaves
/// the music refused (fail closed), not counted as free of video.
/// `register` opens and registers the store. A `register` that failed
/// before it opened anything ([`RegisterFailed::BeforeOpen`]) gives the
/// claim back too.
#[cfg(any(feature = "mobile", test))]
pub fn register_video_root(
    root: &std::path::Path,
    once: &OpenOnce,
    install_hook: impl FnOnce(),
    register: impl FnOnce() -> Result<(), RegisterFailed>,
) -> Registration {
    if !once.claim() {
        return Registration::AlreadyOpened;
    }
    if once.first_hook() {
        install_hook();
    }
    if let Err(e) = std::fs::create_dir_all(root) {
        // Nothing was opened on the root: the next caller may try again.
        once.release();
        return Registration::NoFolder(e.to_string());
    }
    match register() {
        Ok(()) => Registration::Registered,
        Err(RegisterFailed::AfterOpen(e)) => Registration::NotRegistered(e),
        Err(RegisterFailed::BeforeOpen(e)) => {
            // No store was opened on the root: the next caller may try again.
            once.release();
            Registration::NotOpened(e)
        }
    }
}

/// What the music cache is told about the video store: what it holds, still
/// owes, and whether it knows its own size. With no store registered (its
/// folder could not be created, or it has yet to be) nothing is known about
/// what the video holds on disk, so the size is unknown and the music cache
/// admits nothing, rather than reading the absence as zero usage.
#[cfg(any(feature = "mobile", test))]
pub fn outside_bytes(
    store: Option<&dyn super::kept_video::VideoStore>,
) -> pharos_content::OutsideBytes {
    match store {
        Some(s) => {
            // One snapshot: the verdict and the bytes from the same moment.
            let (held, outstanding, unknown) = s.accounting();
            pharos_content::OutsideBytes {
                held,
                outstanding,
                unknown,
            }
        }
        None => pharos_content::OutsideBytes {
            held: 0,
            outstanding: 0,
            unknown: true,
        },
    }
}

/// The hook the phone registers with the music cache (see `outside_bytes`),
/// over the process's video store slot. The phone registers exactly this
/// (`pharos-phone` cannot host a test, being Android-only).
#[cfg(any(feature = "mobile", test))]
pub fn outside_bytes_hook() -> pharos_content::OutsideUsed {
    outside_bytes_hook_for(&VIDEO_STORE)
}

/// `outside_bytes_hook` over a slot of the caller's: a test gives each case
/// a slot of its own, so none depends on what another put in the global.
#[cfg(any(feature = "mobile", test))]
pub fn outside_bytes_hook_for(
    slot: &'static Slot<dyn super::kept_video::VideoStore>,
) -> pharos_content::OutsideUsed {
    Arc::new(move || outside_bytes(slot.get().as_deref()))
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
    use super::super::test_dir::TmpDir;
    use super::*;

    static RAN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn count() {
        RAN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// A store that failed to register (its folder could not be created)
    /// leaves the video's bytes unknown, not zero: the music cache refuses.
    #[test]
    fn no_registered_video_store_reads_as_unknown_not_empty() {
        let o = outside_bytes(None);
        assert!(o.unknown);
        assert_eq!((o.held, o.outstanding), (0, 0));
    }

    /// The init hook and the first render both register the video store;
    /// only the first may open it.
    #[test]
    fn the_second_registration_does_not_open_the_store_again() {
        let once = OpenOnce::new();
        let opened = std::sync::atomic::AtomicUsize::new(0);
        let register = || {
            if once.claim() {
                opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        register();
        register();
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A registration whose folder could not be created opened nothing, so
    /// a later caller tries again (the hook goes in once); once one reaches
    /// the store, every later call does nothing at all.
    #[test]
    fn a_registration_with_no_folder_is_retried_and_one_that_opened_is_not() {
        use std::cell::Cell;
        // A folder of its own, gone when the test ends (a failure
        // included); the root inside it starts missing.
        let scope = TmpDir::new("register-twice");
        let root = scope.path().join("root");
        assert!(std::fs::write(&root, b"a file where the folder should be").is_ok());
        let once = OpenOnce::new();
        let (hooks, registers) = (Cell::new(0), Cell::new(0));
        let run = || {
            register_video_root(
                &root,
                &once,
                || hooks.set(hooks.get() + 1),
                || {
                    registers.set(registers.get() + 1);
                    Ok(())
                },
            )
        };
        assert!(matches!(run(), Registration::NoFolder(_)));
        assert_eq!(
            (hooks.get(), registers.get()),
            (1, 0),
            "hook once, no store"
        );
        assert!(
            matches!(run(), Registration::NoFolder(_)),
            "a failed folder was not tried again"
        );
        assert_eq!((hooks.get(), registers.get()), (1, 0), "a second hook");
        let _ = std::fs::remove_file(&root);
        assert_eq!(run(), Registration::Registered, "the retry did not open");
        assert_eq!((hooks.get(), registers.get()), (1, 1));
        assert_eq!(run(), Registration::AlreadyOpened);
        assert_eq!(
            (hooks.get(), registers.get()),
            (1, 1),
            "a call after the store opened touched the hook or the store"
        );
    }

    /// A store that opened but was not registered keeps the claim: it may
    /// still be open on the root.
    #[test]
    fn a_registration_that_reached_the_store_keeps_its_claim_on_failure() {
        // A folder of its own, gone when the test ends (a failure
        // included); the root inside it starts missing.
        let scope = TmpDir::new("register-kept");
        let root = scope.path().join("root");
        let once = OpenOnce::new();
        let outcome = register_video_root(
            &root,
            &once,
            || {},
            || Err(RegisterFailed::AfterOpen("taken".to_owned())),
        );
        assert_eq!(outcome, Registration::NotRegistered("taken".to_owned()));
        assert_eq!(
            register_video_root(&root, &once, || {}, || Ok(())),
            Registration::AlreadyOpened
        );
    }

    /// A store whose runtime or thread could not start opened nothing on
    /// the root: the claim is given back and the next caller opens it. The
    /// hook still goes in only once.
    #[test]
    fn a_registration_that_failed_before_the_store_opened_is_retried() {
        use std::cell::Cell;
        // A folder of its own, gone when the test ends (a failure
        // included); the root inside it starts missing.
        let scope = TmpDir::new("register-unopened");
        let root = scope.path().join("root");
        let once = OpenOnce::new();
        let hooks = Cell::new(0);
        let outcome = register_video_root(
            &root,
            &once,
            || hooks.set(hooks.get() + 1),
            || Err(RegisterFailed::BeforeOpen("video thread: no".to_owned())),
        );
        assert_eq!(
            outcome,
            Registration::NotOpened("video thread: no".to_owned())
        );
        assert_eq!(
            register_video_root(&root, &once, || hooks.set(hooks.get() + 1), || Ok(())),
            Registration::Registered,
            "a store that never opened kept the claim"
        );
        assert_eq!(hooks.get(), 1, "a second hook");
    }

    /// Two attempts overlap: the second finds the root claimed while the
    /// first is opening, and is not done, since nothing is published. The
    /// first then fails before it opens and gives the claim back; a later
    /// retry opens and registers the store, and only then is it done.
    #[test]
    fn an_attempt_that_found_the_store_opening_is_not_done_until_one_is_published() {
        use std::cell::{Cell, RefCell};
        // A folder of its own, gone when the test ends (a failure
        // included); the root inside it starts missing.
        let scope = TmpDir::new("register-overlap");
        let root = scope.path().join("root");
        let once = OpenOnce::new();
        let published = Cell::new(false);
        let second = RefCell::new(None);
        let first = register_video_root(
            &root,
            &once,
            || {},
            || {
                // The service's retry, while this caller is opening.
                let outcome = register_video_root(&root, &once, || {}, || Ok(()));
                let done = registration_done(&outcome, published.get());
                *second.borrow_mut() = Some((outcome, done));
                Err(RegisterFailed::BeforeOpen("video thread: no".to_owned()))
            },
        );
        assert!(matches!(first, Registration::NotOpened(_)), "{first:?}");
        assert!(!registration_done(&first, published.get()));
        assert_eq!(
            second.take(),
            Some((Registration::AlreadyOpened, false)),
            "an attempt that found the store opening said it was done"
        );
        let retry = register_video_root(
            &root,
            &once,
            || {},
            || {
                published.set(true);
                Ok(())
            },
        );
        assert_eq!(retry, Registration::Registered);
        assert!(registration_done(&retry, published.get()));
    }

    /// On success the hook goes in before the folder is created, and the
    /// store is registered after it: the hook sees no folder, the store
    /// sees one.
    #[test]
    fn the_hook_goes_in_before_the_folder_exists_and_the_store_after() {
        use std::cell::Cell;
        // A folder of its own, gone when the test ends (a failure
        // included); the root inside it starts missing.
        let scope = TmpDir::new("register-order");
        let root = scope.path().join("root");
        let once = OpenOnce::new();
        let (hook_saw_folder, register_saw_folder) = (Cell::new(None), Cell::new(None));
        let outcome = register_video_root(
            &root,
            &once,
            || hook_saw_folder.set(Some(root.exists())),
            || {
                register_saw_folder.set(Some(root.is_dir()));
                Ok(())
            },
        );
        assert_eq!(outcome, Registration::Registered);
        assert_eq!(
            hook_saw_folder.get(),
            Some(false),
            "the hook came after the folder"
        );
        assert_eq!(
            register_saw_folder.get(),
            Some(true),
            "the store came before the folder"
        );
        assert!(root.is_dir());
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
