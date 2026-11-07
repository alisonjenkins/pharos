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

/// File name for a download. The language comes last so
/// `sidecar_language_from_name` reads it, and the file id keeps two results in
/// one language apart.
pub fn file_name(language: &str, file_id: u64) -> String {
    let lang: String = language
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || *c == '-')
        .take(8)
        .collect();
    let lang = if lang.is_empty() { "und" } else { &lang };
    format!("os{file_id}.{lang}.srt")
}

/// Distinguishes temp names within one process; the pid distinguishes them
/// across processes sharing the cache volume.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` for the media at `media_path`, atomically (temp file then
/// rename), and return the final path. The only writer of downloaded
/// subtitles: it cannot name a path outside `root`.
///
/// The temp name is unique per call, so two saves of the same file cannot
/// write into one temp file and rename each other's half-written bytes.
pub async fn save(
    root: &Path,
    media_path: &Path,
    language: &str,
    file_id: u64,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let dir = item_dir(root, media_path);
    tokio::fs::create_dir_all(&dir).await?;
    let name = file_name(language, file_id);
    let final_path = dir.join(&name);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{seq}.part", std::process::id()));
    let result = async {
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &final_path).await
    }
    .await;
    if result.is_err() {
        // Best effort: a failed save must not leave a partial temp file behind.
        let _ = tokio::fs::remove_file(&tmp).await;
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

    #[test]
    fn file_name_ends_in_a_language_the_sidecar_reader_understands() {
        let name = file_name("pt-BR", 4242);
        assert_eq!(name, "os4242.pt-BR.srt");
        assert_eq!(
            crate::api::jellyfin::subtitles::sidecar_language_from_name(&name).as_deref(),
            Some("pt-BR")
        );
    }

    #[test]
    fn hostile_language_cannot_escape_the_directory() {
        let name = file_name("../../etc/passwd", 1);
        assert!(!name.contains('/') && !name.contains(".."), "{name}");
        assert_eq!(file_name("", 1), "os1.und.srt");
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
                save(&root, &media, "en", 7, &a),
                save(&root, &media, "en", 7, &b)
            );
            let final_path = ra.unwrap();
            rb.unwrap();
            let got = tokio::fs::read(&final_path).await.unwrap();
            assert!(got == a || got == b, "final file is one whole payload");
            let dir = final_path.parent().unwrap();
            let names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names.len(), 1, "no temp file left: {names:?}");
        }
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
            .collect();
        assert_eq!(dir_entries.len(), 1, "no temp file left behind");
    }
}
