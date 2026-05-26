//! Recursive media filesystem scan. Generic over `Prober` (V12).
//! Walk lives in `spawn_blocking` — never parks async runtime (V5).

use pharos_core::{DomainError, DomainResult, MediaItem, MediaStore, Prober, Scanner};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flac", "mp3", "opus", "m4a", "ogg", "wav",
];

/// SIMD-accelerated stable ID for a path. xxh3_64 hashes UTF-8 bytes.
pub fn stable_id(path: &Path) -> u64 {
    xxh3_64(path.to_string_lossy().as_bytes())
}

#[derive(Debug, Clone)]
pub struct FsScanner<P: Prober> {
    prober: P,
    extensions: HashSet<String>,
}

impl<P: Prober> FsScanner<P> {
    pub fn new(prober: P) -> Self {
        Self {
            prober,
            extensions: DEFAULT_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }

    pub fn with_extensions(prober: P, exts: impl IntoIterator<Item = String>) -> Self {
        Self {
            prober,
            extensions: exts.into_iter().collect(),
        }
    }

    /// Scan and push items into the given store. Streaming variant — avoids
    /// holding the entire library in memory. V10 atomicity holds per `put`.
    #[tracing::instrument(skip(self, store), fields(root = %root.display()))]
    pub async fn scan_into<S: MediaStore>(&self, root: &Path, store: &S) -> DomainResult<usize> {
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let mut n = 0;
        for p in paths {
            if let Some(item) = self.probe_one(p).await {
                store.put(item).await?;
                n += 1;
            }
        }
        Ok(n)
    }

    async fn probe_one(&self, path: PathBuf) -> Option<MediaItem> {
        match self.prober.probe(&path).await {
            Ok(info) => {
                let title = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                Some(MediaItem {
                    id: stable_id(&path),
                    path,
                    title,
                    kind: info.kind,
                })
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "probe failed, skipping");
                None
            }
        }
    }
}

impl<P: Prober + Clone + 'static> Scanner for FsScanner<P> {
    #[tracing::instrument(skip(self), fields(root = %root.display()))]
    async fn scan(&self, root: &Path) -> DomainResult<Vec<MediaItem>> {
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let mut items = Vec::with_capacity(paths.len());
        for p in paths {
            if let Some(item) = self.probe_one(p).await {
                items.push(item);
            }
        }
        Ok(items)
    }
}

/// Recursive walk inside `spawn_blocking`. Returns paths of files whose
/// lowercased extension is in `exts`.
async fn walk(root: PathBuf, exts: HashSet<String>) -> DomainResult<Vec<PathBuf>> {
    tokio::task::spawn_blocking(move || -> DomainResult<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let e = entry.map_err(|err| DomainError::Backend(err.to_string()))?;
            if !e.file_type().is_file() {
                continue;
            }
            let lower = e
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase());
            if let Some(ext) = lower {
                if exts.contains(&ext) {
                    out.push(e.into_path());
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| DomainError::Backend(format!("walk join: {e}")))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{MediaKind, ProbeInfo};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct FakeProber {
        calls: Arc<AtomicUsize>,
        force_fail_for: Option<String>,
    }

    impl Prober for FakeProber {
        async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(needle) = &self.force_fail_for {
                if path.to_string_lossy().contains(needle.as_str()) {
                    return Err(DomainError::Backend("forced fail".into()));
                }
            }
            let kind = match path.extension().and_then(|s| s.to_str()) {
                Some("flac") | Some("mp3") | Some("opus") | Some("m4a") | Some("ogg")
                | Some("wav") => MediaKind::Audio,
                _ => MediaKind::Movie,
            };
            Ok(ProbeInfo {
                kind,
                duration_ms: None,
                container: None,
            })
        }
    }

    async fn touch(dir: &Path, name: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, b"").await.unwrap();
    }

    #[tokio::test]
    async fn finds_recognized_extensions_and_skips_others() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "movie.mkv").await;
        touch(td.path(), "song.flac").await;
        touch(td.path(), "notes.txt").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        let titles: Vec<_> = items.iter().map(|i| i.title.clone()).collect();
        assert_eq!(items.len(), 2, "got {titles:?}");
        let kinds: HashSet<MediaKind> = items.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&MediaKind::Movie));
        assert!(kinds.contains(&MediaKind::Audio));
    }

    #[tokio::test]
    async fn empty_dir_returns_empty() {
        let td = TempDir::new().unwrap();
        let s = FsScanner::new(FakeProber::default());
        assert!(s.scan(td.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recurses_subdirs() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "top.mkv").await;
        touch(td.path(), "show/season1/ep1.mkv").await;
        touch(td.path(), "show/season1/ep2.mkv").await;
        touch(td.path(), "music/album/track.flac").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 4);
    }

    #[tokio::test]
    async fn probe_failure_is_logged_and_skipped() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "good.mkv").await;
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
        };
        let s = FsScanner::new(prober.clone());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "good");
        assert_eq!(prober.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stable_id_is_deterministic() {
        let a = stable_id(Path::new("/srv/media/movie.mkv"));
        let b = stable_id(Path::new("/srv/media/movie.mkv"));
        assert_eq!(a, b);
        let c = stable_id(Path::new("/srv/media/other.mkv"));
        assert_ne!(a, c);
    }
}
