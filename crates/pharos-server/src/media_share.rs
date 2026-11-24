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
/// a media file. `stale_handle` and `not_connected` name share faults;
/// `io` stays the catch-all it always was.
pub fn unreadable_reason(e: &io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(ESTALE) => "stale_handle",
        Some(ENOTCONN) => "not_connected",
        _ => match e.kind() {
            io::ErrorKind::NotFound => "missing",
            io::ErrorKind::PermissionDenied => "permission",
            _ => "io",
        },
    }
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

/// The share's state, shared by every request: once broken it stays broken
/// until the process ends.
#[derive(Debug)]
pub struct MediaShare {
    roots: Mutex<Vec<PathBuf>>,
    timeout: Duration,
    /// Set while a listing runs; one that never returns keeps it set, so a
    /// hung mount costs one thread, not one per probe.
    busy: Arc<AtomicBool>,
    broken: Mutex<Option<String>>,
}

impl MediaShare {
    pub fn new(timeout: Duration) -> Self {
        Self {
            roots: Mutex::default(),
            timeout,
            busy: Arc::new(AtomicBool::new(false)),
            broken: Mutex::default(),
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
        let mut broken = self.broken.lock().unwrap_or_else(|p| p.into_inner());
        if broken.is_none() {
            tracing::error!(error = %why, "media share broken; the pod needs a fresh mount");
            metrics::gauge!(MEDIA_SHARE_OK).set(0.0);
            *broken = Some(why);
        }
    }

    /// Whether a read has found the share broken.
    pub fn is_broken(&self) -> bool {
        self.broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// `Ok` while no read found the share broken and every root lists.
    pub async fn check(&self) -> Result<(), ShareError> {
        if let Some(why) = self
            .broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Err(ShareError::Broken(why));
        }
        let roots = self.roots.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if roots.is_empty() {
            return Ok(());
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(ShareError::StillWaiting);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let busy = Arc::clone(&self.busy);
        let spawned = std::thread::Builder::new()
            .name("media-share-check".to_owned())
            .spawn(move || {
                let result = roots.iter().try_for_each(|root| list(root));
                busy.store(false, Ordering::Release);
                drop(tx.send(result));
            });
        if let Err(e) = spawned {
            self.busy.store(false, Ordering::Release);
            return Err(ShareError::Spawn(e));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(ShareError::TimedOut(self.timeout)),
        }
    }
}

/// Stats `root` and reads one entry. Only a share-level error fails: a
/// missing or refused root is logged and passes, since a fresh mount would
/// not change it.
fn list(root: &Path) -> Result<(), ShareError> {
    let looked = std::fs::metadata(root).and_then(|_| {
        let mut entries = std::fs::read_dir(root)?;
        entries.next().transpose().map(|_| ())
    });
    match looked {
        Ok(()) => Ok(()),
        Err(e) if breaks_the_share(&e) => Err(ShareError::Io {
            root: root.to_owned(),
            source: e,
        }),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "media root unreadable; not a share fault, staying ready");
            Ok(())
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
                "io"
            ]
        );
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
    async fn a_missing_root_does_not_make_the_server_unready() {
        let share = MediaShare::new(ROOT_TIMEOUT);
        share.set_roots(vec![PathBuf::from("/no/such/media/root")]);
        assert!(share.check().await.is_ok());
    }

    #[tokio::test]
    async fn no_roots_is_ready() {
        assert!(MediaShare::new(ROOT_TIMEOUT).check().await.is_ok());
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
