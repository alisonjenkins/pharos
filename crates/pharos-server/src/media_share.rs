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

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 1 while every media root lists and no read found the share broken, 0 once
/// `/readyz` found otherwise.
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
}

/// Whether the share broke, and what to tell when it does.
#[derive(Default)]
struct Latch {
    broken: Mutex<Option<String>>,
    /// Set once a check found every root listing with the share whole: a pod
    /// that never read its share has nothing to keep serving (ADR-0156).
    /// Written only while `broken` is locked, so a fault and a whole check
    /// cannot interleave.
    seen_whole: AtomicBool,
    /// Told once, when the share first breaks after it was seen whole.
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
        let whole = self.seen_whole.load(Ordering::Acquire);
        // Under the lock: a probe that sees the cause also sees what the
        // hook decided.
        match self.on_broken.get() {
            Some(f) if whole => f(&why),
            Some(_) => tracing::warn!(
                error = %why,
                "media share broke before any check found it whole; not telling the hook"
            ),
            None => {}
        }
        *broken = Some(why);
    }

    /// With a self-restart hook, a root that fails with a share-level error
    /// is the share broken, as a read finding it would be: only a fresh
    /// mount lists it again. Without one, nothing is latched.
    fn note_listing(&self, listed: &Result<bool, ShareError>) {
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
            .field("seen_whole", &self.was_whole())
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
        }
    }

    /// Calls `f` with the cause the first time the share breaks, if a check
    /// had found it whole before (the server's self-restart, ADR-0156). Only
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

    /// Whether a check has found the share whole since the process started.
    pub fn was_whole(&self) -> bool {
        self.latch.seen_whole.load(Ordering::Acquire)
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

    /// `Ok` while no read found the share broken and every root lists.
    pub async fn check(&self) -> Result<(), ShareError> {
        self.check_with(|roots| {
            roots
                .iter()
                .try_fold(true, |all, root| Ok(list(root)? && all))
        })
        .await
    }

    /// `list_roots` answers whether every root listed, as opposed to some
    /// being missing or refused (tolerated, but not the share seen whole).
    async fn check_with(
        &self,
        list_roots: impl FnOnce(&[PathBuf]) -> Result<bool, ShareError> + Send + 'static,
    ) -> Result<(), ShareError> {
        self.check_within(self.timeout, list_roots).await
    }

    /// [`Self::check_with`], waiting at most `timeout` for the listing.
    async fn check_within(
        &self,
        timeout: Duration,
        list_roots: impl FnOnce(&[PathBuf]) -> Result<bool, ShareError> + Send + 'static,
    ) -> Result<(), ShareError> {
        if let Some(why) = self.broken_cause() {
            return Err(ShareError::Broken(why));
        }
        let roots = self.roots.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if roots.is_empty() {
            // No share to read, so none seen whole.
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
        let all_listed = listed?;
        // A read may have found the share broken while the roots listed;
        // decide under the same lock a fault takes.
        let broken = self.latch.broken.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(why) = broken.as_ref() {
            return Err(ShareError::Broken(why.clone()));
        }
        if all_listed {
            self.latch.seen_whole.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn broken_cause(&self) -> Option<String> {
        self.latch
            .broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Stats `root` and reads one entry; `true` when it listed. Only a
/// share-level error fails: a missing or refused root is logged and passes
/// as `false`, since a fresh mount would not change it.
fn list(root: &Path) -> Result<bool, ShareError> {
    let looked = std::fs::metadata(root).and_then(|_| {
        let mut entries = std::fs::read_dir(root)?;
        entries.next().transpose().map(|_| ())
    });
    match looked {
        Ok(()) => Ok(true),
        Err(e) if breaks_the_share(&e) => Err(ShareError::Io {
            root: root.to_owned(),
            source: e,
        }),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "media root unreadable; not a share fault, staying ready");
            Ok(false)
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
        let dir = tempfile::tempdir().unwrap();
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
            Ok(true)
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
    /// share-level error listing a root is a fault too; a whole check is
    /// remembered.
    #[tokio::test]
    async fn the_first_fault_tells_the_hook_once_and_a_listing_fault_counts() {
        let share: &'static MediaShare = Box::leak(Box::new(MediaShare::new(ROOT_TIMEOUT)));
        let told = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&told);
        share.on_broken(move |why| sink.lock().unwrap().push(why.to_owned()));
        share.set_roots(vec![PathBuf::from("/m")]);
        assert!(!share.was_whole());
        assert!(share.check_with(|_| Ok(false)).await.is_ok());
        assert!(
            !share.was_whole(),
            "a tolerated root is not the share whole"
        );
        assert!(share.check_with(|_| Ok(true)).await.is_ok());
        assert!(share.was_whole());

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

    /// A fault before any whole check does not tell the hook: the pod never
    /// read its share, so a restart has nothing to keep serving.
    #[tokio::test]
    async fn a_fault_before_the_share_was_whole_does_not_tell_the_hook() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let told = Arc::new(Mutex::new(0_u32));
        let sink = Arc::clone(&told);
        share.on_broken(move |_| *sink.lock().unwrap() += 1);
        share.note_read_error(Path::new("/m/a"), &err(ESTALE));
        assert!(share.is_broken());
        assert_eq!(*told.lock().unwrap(), 0);
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
        assert!(share.check_with(|_| Ok(true)).await.is_ok());
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
        assert!(share.check_with(|_| Ok(true)).await.is_ok());

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

    #[tokio::test]
    async fn a_missing_root_does_not_make_the_server_unready() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.set_roots(vec![PathBuf::from("/no/such/media/root")]);
        assert!(share.check().await.is_ok());
        assert!(!share.was_whole(), "a missing root was not read");
    }

    #[tokio::test]
    async fn no_roots_is_ready() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        assert!(share.check().await.is_ok());
        assert!(!share.was_whole(), "no share, none seen whole");
    }

    #[tokio::test]
    async fn a_listing_still_running_fails_the_next_check_without_a_thread() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        let dir = tempfile::tempdir().unwrap();
        share.set_roots(vec![dir.path().to_owned()]);
        share.busy.store(true, Ordering::Release);
        assert!(matches!(share.check().await, Err(ShareError::StillWaiting)));
        share.busy.store(false, Ordering::Release);
        assert!(share.check().await.is_ok());
    }
}
