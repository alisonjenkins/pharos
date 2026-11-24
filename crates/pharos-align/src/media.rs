//! Whether the media root can still be read, for `/readyz`.
//!
//! A network mount can go bad under a running pod: the NFS server re-exports
//! the share with a new file handle and every lookup answers `ESTALE` until
//! the mount is made again. Each song then answers 404 although the file is
//! there. Restarting the container keeps the pod's mount, so only a new pod
//! heals it; `/readyz` failing is the signal that asks for one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long one look at the media root may take before it counts as failed.
/// A `hard` NFS mount whose server is gone blocks a `stat` for minutes; a
/// healthy one answers in milliseconds, so two seconds is generous.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the media root could not be read.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("reading {root}: {source}")]
    Io { root: PathBuf, source: io::Error },
    #[error("{root} is not a directory")]
    NotADirectory { root: PathBuf },
    #[error("reading {root} took longer than {timeout:?}")]
    TimedOut { root: PathBuf, timeout: Duration },
    #[error("an earlier read of {root} has not returned")]
    StillWaiting { root: PathBuf },
    #[error("starting the media check thread: {0}")]
    Spawn(io::Error),
}

/// Looks at the media root on its own thread, so a hung mount cannot take an
/// actix worker or the blocking pool that `/align` needs.
#[derive(Debug, Clone)]
pub struct MediaProbe {
    root: PathBuf,
    timeout: Duration,
    /// Set while a look is running; a look that never returns keeps it set,
    /// so a hung mount costs one thread, not one per probe.
    busy: Arc<AtomicBool>,
}

impl MediaProbe {
    pub fn new(root: PathBuf) -> Self {
        Self::with_timeout(root, CHECK_TIMEOUT)
    }

    pub fn with_timeout(root: PathBuf, timeout: Duration) -> Self {
        Self {
            root,
            timeout,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `Ok` when the root is a directory whose listing can be read.
    pub async fn check(&self) -> Result<(), MediaError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(MediaError::StillWaiting {
                root: self.root.clone(),
            });
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let root = self.root.clone();
        let busy = Arc::clone(&self.busy);
        let spawned = std::thread::Builder::new()
            .name("media-check".to_owned())
            .spawn(move || {
                let result = look(&root);
                busy.store(false, Ordering::Release);
                drop(tx.send(result));
            });
        if let Err(e) = spawned {
            self.busy.store(false, Ordering::Release);
            return Err(MediaError::Spawn(e));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(MediaError::TimedOut {
                root: self.root.clone(),
                timeout: self.timeout,
            }),
        }
    }
}

/// Stats the root and reads one entry: a stale handle fails the stat, and
/// a mount that stats from cache but cannot list fails the read.
fn look(root: &Path) -> Result<(), MediaError> {
    let io_err = |source| MediaError::Io {
        root: root.to_owned(),
        source,
    };
    let meta = std::fs::metadata(root).map_err(io_err)?;
    if !meta.is_dir() {
        return Err(MediaError::NotADirectory {
            root: root.to_owned(),
        });
    }
    let mut entries = std::fs::read_dir(root).map_err(io_err)?;
    if let Some(entry) = entries.next() {
        entry.map_err(io_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{MediaError, MediaProbe};
    use std::time::Duration;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pharos-media-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn a_readable_directory_passes_even_when_empty() {
        let d = dir("ok");
        MediaProbe::new(d.clone()).check().await.unwrap();
        std::fs::write(d.join("a"), b"x").unwrap();
        MediaProbe::new(d.clone()).check().await.unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn a_missing_root_fails_with_its_path() {
        let d = dir("gone");
        std::fs::remove_dir_all(&d).unwrap();
        let e = MediaProbe::new(d.clone()).check().await.unwrap_err();
        assert!(matches!(e, MediaError::Io { .. }), "{e}");
        assert!(e.to_string().contains(&d.display().to_string()), "{e}");
    }

    #[tokio::test]
    async fn a_file_root_fails() {
        let d = dir("file");
        let f = d.join("f");
        std::fs::write(&f, b"x").unwrap();
        let e = MediaProbe::new(f).check().await.unwrap_err();
        assert!(matches!(e, MediaError::NotADirectory { .. }), "{e}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn a_look_still_running_fails_the_next_without_a_new_thread() {
        let probe = MediaProbe::with_timeout(dir("busy"), Duration::from_millis(50));
        probe.busy.store(true, std::sync::atomic::Ordering::Release);
        let e = probe.check().await.unwrap_err();
        assert!(matches!(e, MediaError::StillWaiting { .. }), "{e}");
    }
}
