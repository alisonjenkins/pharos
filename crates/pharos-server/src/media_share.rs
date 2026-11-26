//! Whether this pod can still read its media share (B1531, V1531).
//!
//! A stale NFS handle fails every read under the mount, and only a new pod
//! mounts it afresh: a container restart keeps the pod's mount. So a read
//! that fails with a share-level error (stale handle, lost connection, I/O
//! error) marks the share broken for the life of the process, and `/readyz`
//! answers 503 from then on. The chart's `selfHeal` reaper then replaces the
//! pod (ADR-0154). `/readyz` also lists each media root on a thread of its
//! own, at most [`ROOT_TIMEOUT`], since a hard mount whose server is gone
//! blocks a read for good.
//!
//! A root that is missing or refused is NOT a broken share: a library folder
//! deleted or re-permissioned must not take the whole server out of its
//! Service. Only the errors a fresh mount fixes count.
//!
//! When no root holds a single entry, though, the share is absent (B1538,
//! V1538): an export whose disks are gone serves its bare, empty mountpoint,
//! and every catalogued file reads as missing. The pod stays Ready (browsing
//! needs no share) and a fresh mount would be just as empty, so nothing is
//! latched or restarted; each check says so until a root has entries again.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 1 while every media root lists, some root holds entries and no read found
/// the share broken, 0 once `/readyz` found otherwise.
pub const MEDIA_SHARE_OK: &str = "pharos_media_share_ok";

/// How long `/readyz` waits for the media roots to list.
pub const ROOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Linux errno values for a share no longer reachable as mounted.
const ESTALE: i32 = 116;
const EIO: i32 = 5;
const ENOTCONN: i32 = 107;

/// The `pharos_source_unreadable_total` reason for a failed open or read of
/// a media file. `stale_handle`, `not_connected` and `io_error` (EIO) name
/// share faults, the same strings `pharos_cache::hls_cache::source_open_failure`
/// gives; `io` stays the catch-all it always was.
pub fn unreadable_reason(e: &io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(ESTALE) => "stale_handle",
        Some(ENOTCONN) => "not_connected",
        Some(EIO) => "io_error",
        _ => match e.kind() {
            io::ErrorKind::NotFound => "missing",
            io::ErrorKind::PermissionDenied => "permission",
            _ => "io",
        },
    }
}

/// Whether an unreadable reason names a share fault (see [`unreadable_reason`]).
pub fn is_share_reason(reason: &str) -> bool {
    matches!(reason, "stale_handle" | "not_connected" | "io_error")
}

/// Whether `e` says the share itself is gone, so only a fresh mount reads it.
pub fn breaks_the_share(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ESTALE | EIO | ENOTCONN))
}

/// Why `/readyz` finds the share unusable.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    /// A media read failed with a share-level error; kept until the pod ends.
    #[error("{0}")]
    Broken(String),
    #[error("reading media root {root}: {source}")]
    Io { root: PathBuf, source: io::Error },
    #[error("listing the media roots took longer than {0:?}")]
    TimedOut(Duration),
    #[error("an earlier listing of the media roots has not returned")]
    StillWaiting,
    #[error("starting the media check thread: {0}")]
    Spawn(io::Error),
    /// No root holds an entry: the share serves an empty mountpoint.
    #[error("the media share is absent: no media root holds any entry ({empty} empty, {unreadable} missing or refused)")]
    Absent { empty: usize, unreadable: usize },
}

/// What one listing of every media root found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Listed {
    /// Roots that listed with at least one entry.
    pub with_entries: usize,
    /// Roots that listed with none.
    pub empty: usize,
    /// Roots missing or refused (tolerated, see [`list`]).
    pub unreadable: usize,
}

impl Listed {
    fn add(mut self, root: RootListing) -> Self {
        match root {
            RootListing::Entries => self.with_entries += 1,
            RootListing::Empty => self.empty += 1,
            RootListing::Unreadable => self.unreadable += 1,
        }
        self
    }

    /// Roots were listed and none holds an entry.
    fn absent(&self) -> bool {
        self.with_entries == 0 && self.empty + self.unreadable > 0
    }
}

/// How one media root looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootListing {
    Entries,
    Empty,
    Unreadable,
}

/// What [`MediaShare::on_broken`] registers.
type BrokenHook = Box<dyn Fn(&str) + Send + Sync>;

/// The share's state, shared by every request: once broken it stays broken
/// until the process ends.
pub struct MediaShare {
    roots: Mutex<Vec<PathBuf>>,
    timeout: Duration,
    /// Set while a listing runs; one that never returns keeps it set, so a
    /// hung mount costs one thread, not one per probe.
    busy: Arc<AtomicBool>,
    /// Shared with the listing thread, so a listing that fails after its
    /// probe stopped waiting still marks the share broken.
    latch: Arc<Latch>,
    /// Whether the last completed check found no root with an entry.
    absent: AtomicBool,
}

/// Whether the share broke, and what to tell when it does.
#[derive(Default)]
struct Latch {
    broken: Mutex<Option<String>>,
    /// Told once, when the share first breaks.
    on_broken: OnceLock<BrokenHook>,
}

impl Latch {
    fn mark_broken(&self, why: String) {
        let mut broken = self.broken.lock().unwrap_or_else(|p| p.into_inner());
        if broken.is_some() {
            return;
        }
        tracing::error!(error = %why, "media share broken; the pod needs a fresh mount");
        metrics::gauge!(MEDIA_SHARE_OK).set(0.0);
        // Under the lock: a probe that sees the cause also sees what the
        // hook decided. Told even if no check found the share whole yet: the
        // pod still serves browsing, which a restart keeps up (ADR-0158).
        if let Some(f) = self.on_broken.get() {
            f(&why);
        }
        *broken = Some(why);
    }

    /// With a self-restart hook, a root that fails with a share-level error
    /// is the share broken, as a read finding it would be: only a fresh
    /// mount lists it again. Without one, nothing is latched.
    fn note_listing(&self, listed: &Result<Listed, ShareError>) {
        if let Err(e @ ShareError::Io { .. }) = listed {
            if self.on_broken.get().is_some() {
                self.mark_broken(e.to_string());
            }
        }
    }
}

impl std::fmt::Debug for MediaShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaShare")
            .field("timeout", &self.timeout)
            .field("broken", &self.broken_cause())
            .field("absent", &self.is_absent())
            .finish_non_exhaustive()
    }
}

impl MediaShare {
    pub fn new(timeout: Duration) -> Self {
        Self {
            roots: Mutex::default(),
            timeout,
            busy: Arc::new(AtomicBool::new(false)),
            latch: Arc::default(),
            absent: AtomicBool::new(false),
        }
    }

    /// Calls `f` with the cause the first time the share breaks (the
    /// server's self-restart, ADR-0156, ADR-0158). Only
    /// the first `f` is kept. `f` runs with the share's state locked, so a
    /// probe sees the break and whatever `f` set together; it must not call
    /// back into this share.
    ///
    /// With a hook set, a share-level error listing a root also breaks the
    /// share for good, as a read finding it does. Without one, a listing
    /// error fails only that check (V1531 as it was).
    pub fn on_broken(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        if self.latch.on_broken.set(Box::new(f)).is_err() {
            tracing::warn!("media share: an on_broken hook was already set; keeping the first");
        }
    }

    /// The process's one share state: the mount belongs to the pod, so every
    /// read path reports into the same place.
    pub fn global() -> &'static Self {
        static SHARE: OnceLock<MediaShare> = OnceLock::new();
        SHARE.get_or_init(|| Self::new(ROOT_TIMEOUT))
    }

    /// The roots `/readyz` lists. Empty (no media configured) always passes.
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        *self.roots.lock().unwrap_or_else(|p| p.into_inner()) = roots;
    }

    /// Notes a failed read of `path`; marks the share broken when `e` is a
    /// share-level error. Returns whether it did.
    pub fn note_read_error(&self, path: &Path, e: &io::Error) -> bool {
        if !breaks_the_share(e) {
            return false;
        }
        self.mark_broken(format!("reading {}: {e}", path.display()));
        true
    }

    /// Marks the share broken, keeping the first cause.
    pub fn mark_broken(&self, why: String) {
        self.latch.mark_broken(why);
    }

    /// Whether a read has found the share broken.
    pub fn is_broken(&self) -> bool {
        self.latch
            .broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Whether the last completed check found the share absent: no media
    /// root holds an entry, so a catalogued file that reads as missing is
    /// the share's absence, not the file's.
    pub fn is_absent(&self) -> bool {
        self.absent.load(Ordering::Acquire)
    }

    /// Whether a media read that failed for `reason` (an
    /// [`unreadable_reason`]) is the share's absence: a missing file while
    /// no root holds an entry. Such a read answers 503, not 404 (V1538).
    pub fn explains_missing(&self, reason: &str) -> bool {
        reason == "missing" && self.is_absent()
    }

    /// `Ok` while no read found the share broken, every root lists, and
    /// some root holds an entry.
    pub async fn check(&self) -> Result<(), ShareError> {
        self.check_with(|roots| {
            roots.iter().try_fold(
                Listed::default(),
                |listed, root| Ok(listed.add(list(root)?)),
            )
        })
        .await
    }

    /// `list_roots` answers how each root listed: with entries, empty, or
    /// missing or refused (tolerated).
    async fn check_with(
        &self,
        list_roots: impl FnOnce(&[PathBuf]) -> Result<Listed, ShareError> + Send + 'static,
    ) -> Result<(), ShareError> {
        self.check_within(self.timeout, list_roots).await
    }

    /// [`Self::check_with`], waiting at most `timeout` for the listing.
    async fn check_within(
        &self,
        timeout: Duration,
        list_roots: impl FnOnce(&[PathBuf]) -> Result<Listed, ShareError> + Send + 'static,
    ) -> Result<(), ShareError> {
        if let Some(why) = self.broken_cause() {
            return Err(ShareError::Broken(why));
        }
        let roots = self.roots.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if roots.is_empty() {
            // No share to read.
            return Ok(());
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(ShareError::StillWaiting);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let busy = Arc::clone(&self.busy);
        let latch = Arc::clone(&self.latch);
        let spawned = std::thread::Builder::new()
            .name("media-share-check".to_owned())
            .spawn(move || {
                let result = list_roots(&roots);
                // Here, not in the probe: a listing that fails after the
                // probe timed out must still break the share.
                latch.note_listing(&result);
                busy.store(false, Ordering::Release);
                drop(tx.send(result));
            });
        if let Err(e) = spawned {
            self.busy.store(false, Ordering::Release);
            return Err(ShareError::Spawn(e));
        }
        let listed = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(ShareError::TimedOut(timeout)),
        };
        let listed = listed?;
        self.note_absent(&listed);
        // A read may have found the share broken while the roots listed;
        // decide under the same lock a fault takes.
        let broken = self.latch.broken.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(why) = broken.as_ref() {
            return Err(ShareError::Broken(why.clone()));
        }
        if listed.absent() {
            return Err(ShareError::Absent {
                empty: listed.empty,
                unreadable: listed.unreadable,
            });
        }
        Ok(())
    }

    /// Records whether `listed` found the share absent, logging each change.
    fn note_absent(&self, listed: &Listed) {
        let absent = listed.absent();
        if self.absent.swap(absent, Ordering::AcqRel) == absent {
            return;
        }
        if absent {
            tracing::error!(
                empty = listed.empty,
                unreadable = listed.unreadable,
                "media share absent: no media root holds any entry"
            );
        } else {
            tracing::info!(
                with_entries = listed.with_entries,
                empty = listed.empty,
                unreadable = listed.unreadable,
                "media share back: a media root holds entries again"
            );
        }
    }

    /// Sets [`MEDIA_SHARE_OK`] to 1, unless the share is broken. Under the
    /// lock a fault takes, so a fault's 0 is never overwritten.
    pub fn publish_ok(&self) {
        let broken = self.latch.broken.lock().unwrap_or_else(|p| p.into_inner());
        if broken.is_none() {
            metrics::gauge!(MEDIA_SHARE_OK).set(1.0);
        }
    }

    fn broken_cause(&self) -> Option<String> {
        self.latch
            .broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Stats `root` and reads one entry. Only a share-level error fails: a
/// missing or refused root is logged and passes as unreadable, since a fresh
/// mount would not change it.
fn list(root: &Path) -> Result<RootListing, ShareError> {
    let looked = std::fs::metadata(root).and_then(|_| {
        let mut entries = std::fs::read_dir(root)?;
        entries.next().transpose().map(|first| first.is_some())
    });
    match looked {
        Ok(true) => Ok(RootListing::Entries),
        Ok(false) => Ok(RootListing::Empty),
        Err(e) if breaks_the_share(&e) => Err(ShareError::Io {
            root: root.to_owned(),
            source: e,
        }),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "media root unreadable; not a share fault, staying ready");
            Ok(RootListing::Unreadable)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn err(errno: i32) -> io::Error {
        io::Error::from_raw_os_error(errno)
    }

    /// One root, listed with entries.
    const WHOLE: Listed = Listed {
        with_entries: 1,
        empty: 0,
        unreadable: 0,
    };

    /// A root dir holding one file.
    fn filled_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mkv"), b"x").unwrap();
        dir
    }

    #[test]
    fn share_errors_break_the_share_and_file_errors_do_not() {
        assert!(breaks_the_share(&err(ESTALE)));
        assert!(breaks_the_share(&err(EIO)));
        assert!(breaks_the_share(&err(ENOTCONN)));
        assert!(!breaks_the_share(&err(2))); // ENOENT
        assert!(!breaks_the_share(&err(13))); // EACCES
    }

    #[test]
    fn reasons_name_the_share_faults_and_are_distinct() {
        let reasons = [
            unreadable_reason(&err(ESTALE)),
            unreadable_reason(&err(ENOTCONN)),
            unreadable_reason(&err(2)),
            unreadable_reason(&err(13)),
            unreadable_reason(&err(EIO)),
        ];
        assert_eq!(
            reasons,
            [
                "stale_handle",
                "not_connected",
                "missing",
                "permission",
                "io_error"
            ]
        );
        assert_eq!(unreadable_reason(&io::Error::other("x")), "io");
        let share_faults = [ESTALE, ENOTCONN, EIO, 2, 13]
            .map(|errno| is_share_reason(unreadable_reason(&err(errno))));
        assert_eq!(share_faults, [true, true, true, false, false]);
        assert!(!is_share_reason("io"));
    }

    #[tokio::test]
    async fn a_broken_share_stays_unready_and_keeps_its_first_cause() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let dir = filled_root();
        share.set_roots(vec![dir.path().to_owned()]);
        assert!(share.check().await.is_ok());

        assert!(!share.note_read_error(Path::new("/m/a"), &err(2)));
        assert!(
            share.check().await.is_ok(),
            "a missing file is not a share fault"
        );

        assert!(share.note_read_error(Path::new("/m/a"), &err(ESTALE)));
        share.note_read_error(Path::new("/m/b"), &err(EIO));
        let e = share.check().await.unwrap_err();
        assert!(matches!(e, ShareError::Broken(_)));
        assert!(e.to_string().contains("/m/a"), "{e}");
        assert!(share.check().await.is_err(), "it stays broken");
    }

    #[tokio::test]
    async fn a_fault_found_while_the_roots_list_fails_that_check() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.set_roots(vec![PathBuf::from("/m")]);
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let listing = share.check_with(move |_| {
            wait.recv().ok();
            Ok(WHOLE)
        });
        let fault = async {
            // Only once the listing is under way, so the check's first look
            // at the broken flag has already passed.
            while !share.busy.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            share.note_read_error(Path::new("/m/a"), &err(ESTALE));
            release.send(()).unwrap();
        };
        let (checked, ()) = tokio::join!(listing, fault);
        assert!(matches!(checked, Err(ShareError::Broken(_))), "{checked:?}");
    }

    /// ADR-0156: the first fault tells the hook once, with its cause; a
    /// share-level error listing a root is a fault too.
    #[tokio::test]
    async fn the_first_fault_tells_the_hook_once_and_a_listing_fault_counts() {
        let share: &'static MediaShare = Box::leak(Box::new(MediaShare::new(ROOT_TIMEOUT)));
        let told = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&told);
        share.on_broken(move |why| sink.lock().unwrap().push(why.to_owned()));
        share.set_roots(vec![PathBuf::from("/m")]);
        let tolerated = Listed {
            unreadable: 1,
            ..WHOLE
        };
        assert!(share.check_with(move |_| Ok(tolerated)).await.is_ok());
        assert!(share.check_with(|_| Ok(WHOLE)).await.is_ok());

        let listed = share
            .check_with(|roots| {
                Err(ShareError::Io {
                    root: roots[0].clone(),
                    source: err(ESTALE),
                })
            })
            .await;
        assert!(matches!(listed, Err(ShareError::Io { .. })), "{listed:?}");
        share.note_read_error(Path::new("/m/b"), &err(EIO));
        assert!(matches!(share.check().await, Err(ShareError::Broken(_))));
        let told = told.lock().unwrap();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("/m"), "{told:?}");
    }

    /// ADR-0158: a fault before any check found the share whole (a pod that
    /// started on an absent share) still tells the hook: the pod serves
    /// browsing, and its restart keeps that up while a fresh mount is tried.
    #[tokio::test]
    async fn a_fault_before_any_whole_check_still_tells_the_hook() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let told = Arc::new(Mutex::new(0_u32));
        let sink = Arc::clone(&told);
        share.on_broken(move |_| *sink.lock().unwrap() += 1);
        let empty = tempfile::tempdir().unwrap();
        share.set_roots(vec![empty.path().to_owned()]);
        assert!(matches!(
            share.check().await,
            Err(ShareError::Absent { .. })
        ));
        share.note_read_error(Path::new("/m/a"), &err(ESTALE));
        assert!(share.is_broken());
        assert_eq!(*told.lock().unwrap(), 1);
    }

    /// Without a hook (self-restart off) a listing error fails only that
    /// check, as before ADR-0156.
    #[tokio::test]
    async fn without_a_hook_a_listing_fault_is_not_latched() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.set_roots(vec![PathBuf::from("/m")]);
        let listed = share
            .check_with(|roots| {
                Err(ShareError::Io {
                    root: roots[0].clone(),
                    source: err(ESTALE),
                })
            })
            .await;
        assert!(matches!(listed, Err(ShareError::Io { .. })), "{listed:?}");
        assert!(!share.is_broken());
        assert!(share.check_with(|_| Ok(WHOLE)).await.is_ok());
    }

    /// A listing that fails with a share fault only after its probe gave up
    /// still breaks the share and tells the hook, once.
    #[tokio::test]
    async fn a_listing_fault_after_the_probe_timed_out_still_tells_the_hook() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let told = Arc::new(Mutex::new(0_u32));
        let sink = Arc::clone(&told);
        share.on_broken(move |_| *sink.lock().unwrap() += 1);
        share.set_roots(vec![PathBuf::from("/m")]);
        assert!(share.check_with(|_| Ok(WHOLE)).await.is_ok());

        let (release, wait) = std::sync::mpsc::channel::<()>();
        let listed = share
            .check_within(Duration::from_millis(10), move |roots| {
                wait.recv().ok();
                Err(ShareError::Io {
                    root: roots[0].clone(),
                    source: err(EIO),
                })
            })
            .await;
        assert!(matches!(listed, Err(ShareError::TimedOut(_))), "{listed:?}");
        release.send(()).unwrap();
        while share.busy.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(share.is_broken());
        assert_eq!(*told.lock().unwrap(), 1);
        assert!(matches!(share.check().await, Err(ShareError::Broken(_))));
        assert_eq!(*told.lock().unwrap(), 1);
    }

    /// A healthy check finishing after a fault latched must not set the
    /// gauge back to 1.
    #[test]
    fn publish_ok_leaves_a_latched_fault_at_zero() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let gauge = || {
            recorder
                .snapshotter()
                .snapshot()
                .into_vec()
                .into_iter()
                .find(|(k, ..)| k.key().name() == MEDIA_SHARE_OK)
                .map(|(.., v)| match v {
                    DebugValue::Gauge(g) => g.into_inner(),
                    other => panic!("{other:?}"),
                })
        };
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.publish_ok();
        assert_eq!(gauge(), Some(1.0));
        share.note_read_error(Path::new("/m/a"), &err(EIO));
        share.publish_ok();
        assert_eq!(gauge(), Some(0.0));
    }

    #[tokio::test]
    async fn a_missing_root_is_absent_not_broken() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.set_roots(vec![PathBuf::from("/no/such/media/root")]);
        let checked = share.check().await;
        assert!(
            matches!(
                checked,
                Err(ShareError::Absent {
                    empty: 0,
                    unreadable: 1
                })
            ),
            "{checked:?}"
        );
        assert!(!share.is_broken(), "a fresh mount would not change it");
    }

    /// B1538: an export whose disks are gone serves an empty mountpoint.
    /// With no root holding an entry the share is absent, never broken (a
    /// fresh mount is just as empty, so the hook is not told), and it comes
    /// back by itself once a root holds an entry again.
    #[tokio::test]
    async fn roots_with_no_entries_make_the_share_absent_until_one_has_some() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let told = Arc::new(Mutex::new(0_u32));
        let sink = Arc::clone(&told);
        share.on_broken(move |_| *sink.lock().unwrap() += 1);
        let empty = tempfile::tempdir().unwrap();
        share.set_roots(vec![
            empty.path().to_owned(),
            PathBuf::from("/no/such/media/root"),
        ]);
        let checked = share.check().await;
        assert!(
            matches!(
                checked,
                Err(ShareError::Absent {
                    empty: 1,
                    unreadable: 1
                })
            ),
            "{checked:?}"
        );
        let why = checked.unwrap_err().to_string();
        assert!(why.contains("1 empty, 1 missing or refused"), "{why}");
        assert!(share.is_absent());
        assert!(!share.is_broken());
        assert_eq!(*told.lock().unwrap(), 0);

        std::fs::write(empty.path().join("a.mkv"), b"x").unwrap();
        assert!(share.check().await.is_ok());
        assert!(!share.is_absent(), "a root with entries brings it back");
    }

    #[tokio::test]
    async fn one_root_with_entries_is_not_absent() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let filled = filled_root();
        let empty = tempfile::tempdir().unwrap();
        share.set_roots(vec![
            filled.path().to_owned(),
            empty.path().to_owned(),
            PathBuf::from("/no/such/media/root"),
        ]);
        assert!(share.check().await.is_ok());
        assert!(!share.is_absent());
    }

    #[tokio::test]
    async fn no_roots_is_ready() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        assert!(share.check().await.is_ok());
    }

    #[tokio::test]
    async fn a_listing_still_running_fails_the_next_check_without_a_thread() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let dir = filled_root();
        share.set_roots(vec![dir.path().to_owned()]);
        share.busy.store(true, Ordering::Release);
        assert!(matches!(share.check().await, Err(ShareError::StillWaiting)));
        share.busy.store(false, Ordering::Release);
        assert!(share.check().await.is_ok());
    }
}
