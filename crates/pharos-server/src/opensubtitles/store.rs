//! Where downloaded subtitles live (V564, V565).
//!
//! Always under the root on the cache volume, `<root>/<media id>/`, never
//! beside the media: the library may be a read-only NFS mount. The media id is
//! `stable_id(media path)`, the key `discover_sidecars` can compute from a path
//! alone, so a downloaded file joins the item's external subtitle list without
//! threading state through every caller.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Record the process-wide store root (first call wins). Sidecar discovery
/// reads it; see ADR-0069 for why this is not threaded through its callers.
pub fn install_root(root: &Path) {
    let installed = ROOT.get_or_init(|| root.to_path_buf());
    if installed != root {
        tracing::warn!(
            installed = %installed.display(),
            ignored = %root.display(),
            "opensubtitles store root already installed; keeping the first"
        );
    }
}

pub fn installed_root() -> Option<&'static Path> {
    ROOT.get().map(PathBuf::as_path)
}

/// Directory holding the downloads for the media file at `media_path`.
pub fn item_dir(root: &Path, media_path: &Path) -> PathBuf {
    root.join(format!("{:016x}", pharos_scanner::stable_id(media_path)))
}

/// File name for a download. The language comes before any `sdh` tag so
/// `sidecar_language_from_name` reads it, and the file id keeps two results in
/// one language apart. `sdh` marks a hearing-impaired file the way a sidecar
/// beside the media would be marked, so it is labelled SDH (V1510).
pub fn file_name(language: &str, file_id: u64, sdh: bool) -> String {
    let lang: String = language
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || *c == '-')
        .take(8)
        .collect();
    let lang = if lang.is_empty() { "und" } else { &lang };
    let tag = if sdh { ".sdh" } else { "" };
    format!("os{file_id}.{lang}{tag}.srt")
}

/// Distinguishes temp names within one process; the pid distinguishes them
/// across processes sharing the cache volume.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The arrival number a saved download carries, from its published name
/// `NNNNNNNN-os<id>.<lang>.srt`. `None` for a download saved before the
/// numbers existed. Sidecar discovery lists downloads by this number, so a
/// new download always takes the next index and never moves one a client was
/// already handed (V1509).
pub fn arrival(name: &str) -> Option<u64> {
    let (seq, _) = name.split_once('-')?;
    if seq.len() == ARRIVAL_DIGITS && seq.bytes().all(|b| b.is_ascii_digit()) {
        seq.parse().ok()
    } else {
        None
    }
}

const ARRIVAL_DIGITS: usize = 8;

/// How long a save waits for another save into the same directory.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Holds an exclusive advisory lock (`flock`) on a directory's `.lock` file,
/// which excludes another process sharing the cache volume too. The file is
/// never removed, so no save can delete a lock another holds; the lock goes
/// with the open file, so one left by a save that died is released by the
/// kernel, not taken over by age.
struct DirLock(#[allow(dead_code)] std::fs::File);

impl DirLock {
    async fn take(dir: &Path) -> io::Result<Self> {
        let path = dir.join(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = tokio::time::Instant::now() + LOCK_WAIT;
        loop {
            match try_lock(&file) {
                Ok(()) => return Ok(Self(file)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("subtitle store busy: {}", path.display()),
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// `flock(LOCK_EX | LOCK_NB)`: `std::fs::File::try_lock` is newer than the
/// workspace MSRV. The lock belongs to this open file, so each `take` (a
/// fresh open) excludes every other, in this process or another.
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` reads only the descriptor, which `file` keeps open for
    // the whole call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// No cross-process lock off unix; the server ships for Linux.
#[cfg(not(unix))]
fn try_lock(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

/// Write `bytes` for the media at `media_path`, atomically (temp file then
/// rename), and return the final path. The only writer of downloaded
/// subtitles: it cannot name a path outside `root`.
///
/// Saves into one directory take turns under its lock ([`DirLock`]), so the arrival number
/// a download is published with ([`arrival`]) is always the highest there:
/// publishing order and listing order agree, across processes too. The same
/// OpenSubtitles file is the same bytes, so a save of one already there
/// returns it unchanged and keeps its place (V1509).
pub async fn save(
    root: &Path,
    media_path: &Path,
    language: &str,
    file_id: u64,
    sdh: bool,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let dir = item_dir(root, media_path);
    tokio::fs::create_dir_all(&dir).await?;
    let name = file_name(language, file_id, sdh);
    let lock = DirLock::take(&dir).await?;
    // The scan, write and rename run as one blocking task that owns the
    // lock: a caller cancelled part-way cannot let the lock go while a
    // detached rename is still to land, so no other save can take the same
    // arrival number (V1509).
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || publish_locked(&lock, &dir, &name, &bytes))
        .await
        .map_err(io::Error::other)?
}

/// The part of [`save`] done under the directory's lock: return the copy
/// already there, or publish `bytes` with the next arrival number.
fn publish_locked(_lock: &DirLock, dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let mut next = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(existing) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let seq = arrival(&existing);
        let bare = match seq {
            Some(_) => existing.split_once('-').map_or("", |(_, rest)| rest),
            None => existing.as_str(),
        };
        if bare == name {
            return Ok(entry.path());
        }
        if let Some(seq) = seq {
            next = next.max(seq + 1);
        }
    }
    let final_path = dir.join(format!("{next:0width$}-{name}", width = ARRIVAL_DIGITS));
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{seq}.part", std::process::id()));
    let result = std::fs::write(&tmp, bytes).and_then(|()| {
        #[cfg(test)]
        tests::pause_point(dir);
        std::fs::rename(&tmp, &final_path)
    });
    if result.is_err() {
        // Best effort: a failed save must not leave a partial temp file behind.
        let _ = std::fs::remove_file(&tmp);
    }
    result.map(|()| final_path)
}

/// A process-wide store root for tests: `install_root` is first-wins, so every
/// test in the process must use the same one.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn shared_test_root() -> PathBuf {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir for the shared test root"));
    let root = dir.path().join("opensubtitles");
    install_root(&root);
    root
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Test hook between the write and the rename: holds there while the
    /// directory has a `.pause` file.
    pub(super) fn pause_point(dir: &Path) {
        while dir.join(".pause").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A save cancelled after its write but before its rename keeps the
    /// lock until the rename lands, so the next save takes the next arrival
    /// number rather than the same one.
    #[tokio::test]
    async fn a_cancelled_save_holds_the_lock_until_it_has_published() {
        let td = tempfile::tempdir().unwrap();
        let media = td.path().join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache");
        let dir = item_dir(&root, &media);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let pause = dir.join(".pause");
        tokio::fs::write(&pause, b"").await.unwrap();
        let (r, m) = (root.clone(), media.clone());
        let first = tokio::spawn(async move { save(&r, &m, "en", 1, false, b"a").await });
        // Wait for its temp file: the write is done, the rename is held.
        let part = loop {
            let found = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .find(|n| n.to_string_lossy().ends_with(".part"));
            if let Some(found) = found {
                break found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        first.abort();
        let (r, m) = (root.clone(), media.clone());
        let second = tokio::spawn(async move { save(&r, &m, "en", 2, false, b"b").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !second.is_finished(),
            "the cancelled save still holds the lock"
        );
        tokio::fs::remove_file(&pause).await.unwrap();
        let p = second.await.unwrap().unwrap();
        assert_eq!(arrival(p.file_name().unwrap().to_str().unwrap()), Some(1));
        assert!(dir.join("00000000-os1.en.srt").exists(), "the first landed");
        assert!(!dir.join(part).exists());
    }

    #[test]
    fn file_name_ends_in_a_language_the_sidecar_reader_understands() {
        let name = file_name("pt-BR", 4242, false);
        assert_eq!(name, "os4242.pt-BR.srt");
        assert_eq!(
            crate::api::jellyfin::subtitles::sidecar_language_from_name(&name).as_deref(),
            Some("pt-BR")
        );
        let sdh = file_name("en", 7, true);
        assert_eq!(sdh, "os7.en.sdh.srt");
        assert_eq!(
            crate::api::jellyfin::subtitles::sidecar_language_from_name(&sdh).as_deref(),
            Some("en")
        );
    }

    #[test]
    fn hostile_language_cannot_escape_the_directory() {
        let name = file_name("../../etc/passwd", 1, false);
        assert!(!name.contains('/') && !name.contains(".."), "{name}");
        assert_eq!(file_name("", 1, false), "os1.und.srt");
    }

    /// Two saves of one file race; both succeed and the final file is one
    /// whole payload, never a mix, and no temp file is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_saves_of_one_file_both_succeed_and_leave_it_whole() {
        let td = tempfile::tempdir().unwrap();
        let media = td.path().join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache");
        let a = vec![b'a'; 512 * 1024];
        let b = vec![b'b'; 512 * 1024];
        for _ in 0..20 {
            let (ra, rb) = tokio::join!(
                save(&root, &media, "en", 7, false, &a),
                save(&root, &media, "en", 7, false, &b)
            );
            let final_path = ra.unwrap();
            let got = tokio::fs::read(&final_path).await.unwrap();
            assert!(got == a || got == b, "final file is one whole payload");
            let dir = final_path.parent().unwrap();
            let names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|n| n != ".lock")
                .collect();
            assert_eq!(names.len(), 1, "no temp file left: {names:?}");
            assert_eq!(rb.as_ref().ok(), Some(&final_path), "one copy, one place");
            // A save of a file already there keeps it, so start each round
            // without one to race the two saves again.
            tokio::fs::remove_file(&final_path).await.unwrap();
        }
    }

    #[tokio::test]
    async fn saving_a_file_already_there_keeps_the_first_copy() {
        let td = tempfile::tempdir().unwrap();
        let media = td.path().join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache");
        let first = save(&root, &media, "en", 7, false, b"first").await.unwrap();
        let again = save(&root, &media, "en", 7, false, b"again").await.unwrap();
        assert_eq!(first, again);
        assert_eq!(tokio::fs::read(&first).await.unwrap(), b"first");
    }

    /// Saves racing into one directory publish in turn, each with the next
    /// arrival number, so the listing order is the publishing order.
    #[tokio::test]
    async fn racing_saves_publish_in_arrival_order_with_no_repeats() {
        let td = tempfile::tempdir().unwrap();
        let media = td.path().join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache");
        // Spawn every save before awaiting any, so they run at once.
        let saves: Vec<_> = (0..16u64)
            .map(|id| {
                let (root, media) = (root.clone(), media.clone());
                tokio::spawn(async move { save(&root, &media, "en", id, false, b"x").await })
            })
            .collect();
        let mut seqs = Vec::new();
        for s in saves {
            let p = s.await.unwrap().unwrap();
            seqs.push(arrival(p.file_name().unwrap().to_str().unwrap()).unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (0..16).collect::<Vec<_>>());
        let lock = std::fs::File::open(item_dir(&root, &media).join(".lock")).unwrap();
        assert!(try_lock(&lock).is_ok(), "every save let the lock go");
    }

    /// A save waits while another holder has the directory's lock, and
    /// proceeds once it is let go: removing nothing, so no holder's lock can
    /// be deleted by another.
    #[tokio::test]
    async fn a_save_waits_for_the_directory_lock() {
        let td = tempfile::tempdir().unwrap();
        let media = td.path().join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache");
        let dir = item_dir(&root, &media);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let held = DirLock::take(&dir).await.unwrap();
        let (r, m) = (root.clone(), media.clone());
        let save = tokio::spawn(async move { save(&r, &m, "en", 3, false, b"x").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!save.is_finished(), "the save waits for the held lock");
        drop(held);
        let p = save.await.unwrap().unwrap();
        assert_eq!(arrival(p.file_name().unwrap().to_str().unwrap()), Some(0));
        assert!(dir.join(".lock").exists(), "the lock file is never removed");
    }

    #[test]
    fn arrival_reads_only_the_published_prefix() {
        assert_eq!(arrival("00000003-os7.en.srt"), Some(3));
        assert_eq!(arrival("os7.en.srt"), None, "saved before the numbers");
        assert_eq!(arrival("3-os7.en.srt"), None);
        assert_eq!(arrival("pt-br.srt"), None);
    }

    #[tokio::test]
    async fn save_writes_under_the_root_and_leaves_the_media_dir_alone() {
        let td = tempfile::tempdir().unwrap();
        let media_dir = td.path().join("media");
        tokio::fs::create_dir_all(&media_dir).await.unwrap();
        let media = media_dir.join("film.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = td.path().join("cache").join("opensubtitles");
        let p = save(
            &root,
            &media,
            "en",
            7,
            false,
            b"1\n00:00:01,000 --> 00:00:02,000\nhi\n",
        )
        .await
        .unwrap();
        assert!(p.starts_with(&root));
        assert_eq!(tokio::fs::read(&p).await.unwrap().len(), 35);
        let mut left = tokio::fs::read_dir(&media_dir).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = left.next_entry().await.unwrap() {
            names.push(e.file_name());
        }
        assert_eq!(names, ["film.mkv"], "nothing is written beside the media");
        let dir_entries: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != ".lock")
            .collect();
        assert_eq!(dir_entries.len(), 1, "no temp file left behind");
    }
}
