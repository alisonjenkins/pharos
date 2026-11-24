//! Whether the media root can still be read, for `/readyz`.
//!
//! A network mount can go bad under a running pod: the NFS server re-exports
//! the share with a new file handle and every lookup answers `ESTALE` until
//! the mount is made again. Each song then answers 404 although the file is
//! there. Restarting the container keeps the pod's mount, so only a new pod
//! heals it; `/readyz` failing is the signal that asks for one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long one look at the media root may take before it counts as failed.
/// A `hard` NFS mount whose server is gone blocks a `stat` for minutes; a
/// healthy one answers in milliseconds, so two seconds is generous.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the read of one song's first byte may take. A healthy share
/// answers in milliseconds; a hard mount whose server is gone never does.
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Song reads that may be stuck on the share at once. Past this every new
/// song answers 503 at once rather than parking another thread.
pub const MAX_STUCK_SOURCE_READS: usize = 4;

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
    /// A song's file failed to read on a broken share; kept until the pod
    /// is replaced, since a listing of the root can still succeed.
    #[error("{0}")]
    Broken(String),
}

/// Linux errno values for a share that is no longer reachable as mounted.
const ESTALE: i32 = 116;
const EIO: i32 = 5;
const ENOTCONN: i32 = 107;

/// Why a song's file could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// The share's handle went stale (ESTALE): only a fresh mount reads it.
    StaleHandle,
    /// EIO from the share.
    Io,
    /// The share's connection is gone (ENOTCONN).
    NotConnected,
    /// The read did not answer within [`SOURCE_TIMEOUT`], or too many earlier
    /// reads are still stuck: a hard mount whose server is gone.
    TimedOut,
    /// Any other error, such as a permission refused on one file; the share
    /// itself is not taken to be broken.
    Other,
}

impl Unreadable {
    /// What `err` says of the share.
    pub fn of(err: &io::Error) -> Self {
        match err.raw_os_error() {
            Some(ESTALE) => Self::StaleHandle,
            Some(EIO) => Self::Io,
            Some(ENOTCONN) => Self::NotConnected,
            _ => Self::Other,
        }
    }

    /// The `reason` label of [`crate::obs::SOURCE_UNREADABLE_TOTAL`]: a
    /// stable string for dashboards.
    pub fn label(self) -> &'static str {
        match self {
            Self::StaleHandle => "stale_handle",
            Self::Io => "io",
            Self::NotConnected => "not_connected",
            Self::TimedOut => "timed_out",
            Self::Other => "other",
        }
    }

    /// Whether only a fresh mount reads the share again.
    pub fn breaks_the_share(self) -> bool {
        !matches!(self, Self::Other)
    }
}

/// What reading one song's file found.
#[derive(Debug)]
pub enum Source {
    Readable,
    Missing,
    Unreadable(Unreadable, io::Error),
}

/// Looks the file at `path` up and reads its first byte through the share:
/// a stale handle fails either. Only a file that is not there is missing;
/// `Path::is_file` would read every error as missing.
pub fn read_source(path: &Path) -> Source {
    use std::io::Read as _;
    let opened = std::fs::metadata(path).and_then(|m| {
        if m.is_file() {
            std::fs::File::open(path).map(Some)
        } else {
            Ok(None)
        }
    });
    let mut file = match opened {
        Ok(Some(f)) => f,
        Ok(None) => return Source::Missing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Source::Missing,
        Err(e) => return Source::Unreadable(Unreadable::of(&e), e),
    };
    let mut byte = [0_u8; 1];
    // An empty file reads 0 bytes and is still readable; the decode judges it.
    match file.read(&mut byte) {
        Ok(0 | 1) => Source::Readable,
        Ok(n) => Source::Unreadable(
            Unreadable::Other,
            io::Error::other(format!("read {n} bytes into a 1-byte buffer")),
        ),
        Err(e) => Source::Unreadable(Unreadable::of(&e), e),
    }
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
    /// The first song read that found the share broken, if one has.
    broken: Arc<Mutex<Option<String>>>,
    source_timeout: Duration,
    /// Song reads started and not yet returned.
    source_reads: Arc<AtomicUsize>,
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
            broken: Arc::default(),
            source_timeout: SOURCE_TIMEOUT,
            source_reads: Arc::default(),
        }
    }

    /// The same probe with `timeout` for a song read, for tests.
    pub fn with_source_timeout(mut self, timeout: Duration) -> Self {
        self.source_timeout = timeout;
        self
    }

    /// [`read_source`] on a thread of its own, given at most the source
    /// timeout. A read that never returns keeps its thread, so at most
    /// [`MAX_STUCK_SOURCE_READS`] run at once; past that, and on a timeout,
    /// the song is [`Unreadable::TimedOut`].
    pub async fn read_source(&self, path: PathBuf) -> Source {
        if self.source_reads.fetch_add(1, Ordering::AcqRel) >= MAX_STUCK_SOURCE_READS {
            self.source_reads.fetch_sub(1, Ordering::AcqRel);
            return Source::Unreadable(
                Unreadable::TimedOut,
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{MAX_STUCK_SOURCE_READS} earlier reads of the share have not returned"
                    ),
                ),
            );
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let reads = Arc::clone(&self.source_reads);
        let spawned = std::thread::Builder::new()
            .name("song-read".to_owned())
            .spawn(move || {
                let found = read_source(&path);
                reads.fetch_sub(1, Ordering::AcqRel);
                drop(tx.send(found));
            });
        if let Err(e) = spawned {
            self.source_reads.fetch_sub(1, Ordering::AcqRel);
            return Source::Unreadable(Unreadable::Other, e);
        }
        match tokio::time::timeout(self.source_timeout, rx).await {
            Ok(Ok(found)) => found,
            Ok(Err(_)) | Err(_) => Source::Unreadable(
                Unreadable::TimedOut,
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no answer within {:?}", self.source_timeout),
                ),
            ),
        }
    }

    /// Marks the share broken for good, keeping the first cause: a song's
    /// file failed to read with a stale handle or I/O error, which only a
    /// new pod's mount clears.
    pub fn mark_broken(&self, why: String) {
        let mut broken = self.broken.lock().unwrap_or_else(|p| p.into_inner());
        if broken.is_none() {
            tracing::error!(error = %why, "media share broken; the pod needs a fresh mount");
            *broken = Some(why);
        }
    }

    /// `Ok` when the root is a directory whose listing can be read.
    pub async fn check(&self) -> Result<(), MediaError> {
        if let Some(why) = self
            .broken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Err(MediaError::Broken(why));
        }
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
    use std::sync::atomic::Ordering;
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
    async fn a_share_a_song_found_broken_stays_unready_while_its_root_lists() {
        let d = dir("broken");
        let probe = MediaProbe::new(d.clone());
        probe.check().await.unwrap();
        probe.mark_broken("first".into());
        probe.mark_broken("second".into());
        let e = probe.check().await.unwrap_err();
        assert!(
            matches!(e, MediaError::Broken(ref why) if why == "first"),
            "{e}"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn song_reads_stuck_on_the_share_cap_new_ones_as_timed_out() {
        let d = dir("stuck");
        let song = d.join("song.flac");
        std::fs::write(&song, b"x").unwrap();
        let probe = MediaProbe::new(d.clone());
        assert!(matches!(
            probe.read_source(song.clone()).await,
            super::Source::Readable
        ));
        // As if MAX_STUCK_SOURCE_READS reads were hung on a dead mount.
        probe
            .source_reads
            .store(super::MAX_STUCK_SOURCE_READS, Ordering::Release);
        let found = probe.read_source(song.clone()).await;
        assert!(
            matches!(
                found,
                super::Source::Unreadable(super::Unreadable::TimedOut, _)
            ),
            "{found:?}"
        );
        assert!(super::Unreadable::TimedOut.breaks_the_share());
        probe.source_reads.store(0, Ordering::Release);
        assert!(matches!(
            probe.read_source(song).await,
            super::Source::Readable
        ));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn mount_errors_break_the_share_and_others_do_not() {
        use super::Unreadable;
        let of = |errno| Unreadable::of(&std::io::Error::from_raw_os_error(errno));
        assert_eq!(of(116), Unreadable::StaleHandle);
        assert_eq!(of(5), Unreadable::Io);
        assert_eq!(of(107), Unreadable::NotConnected);
        assert_eq!(of(13), Unreadable::Other);
        assert!(of(116).breaks_the_share());
        assert!(!of(13).breaks_the_share());
        let mut labels = [
            Unreadable::StaleHandle.label(),
            Unreadable::Io.label(),
            Unreadable::NotConnected.label(),
            Unreadable::TimedOut.label(),
            Unreadable::Other.label(),
        ]
        .to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn a_file_reads_and_a_missing_file_or_directory_is_missing() {
        use super::{read_source, Source};
        let d = dir("source");
        let song = d.join("song.mp3");
        std::fs::write(&song, b"x").unwrap();
        assert!(matches!(read_source(&song), Source::Readable));
        assert!(matches!(read_source(&d.join("gone.mp3")), Source::Missing));
        assert!(matches!(read_source(&d), Source::Missing));
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
