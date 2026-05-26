//! Disk-backed Primary image cache.
//!
//! On miss, spawn ffmpeg to extract a JPEG poster from the source
//! media and write it under `{root}/{kind}/{id}.jpg`. On hit, return
//! the path so the HTTP layer can stream it via `NamedFile`.
//!
//! V6 still holds — ffmpeg failures are propagated as `Err`, never
//! propagated as a server crash. Cache writes are atomic (write to
//! `.tmp` then rename) so a kill mid-extract never leaves a corrupted
//! poster.

use pharos_core::MediaKind;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::instrument;

#[derive(Debug, Clone)]
pub struct ImageCache {
    root: PathBuf,
    ffmpeg_bin: PathBuf,
    seek_seconds: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg exit {0:?}: {1}")]
    Ffmpeg(Option<i32>, String),
    #[error("non-utf8 path")]
    NonUtf8Path,
    #[error("unsupported kind for primary extraction")]
    UnsupportedKind,
}

/// Where on disk a poster for the given media id lives.
pub fn primary_path(root: &Path, kind: MediaKind, id: u64) -> PathBuf {
    let dir = match kind {
        MediaKind::Movie => "movie",
        MediaKind::Episode => "episode",
        MediaKind::Audio => "audio",
    };
    root.join(dir).join(format!("{id}.jpg"))
}

impl ImageCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            seek_seconds: 30,
        }
    }

    pub fn with_ffmpeg(mut self, p: impl Into<PathBuf>) -> Self {
        self.ffmpeg_bin = p.into();
        self
    }

    /// Override the decode-seek timestamp used when extracting a
    /// poster from video / episode sources. Defaults to 30s, which
    /// works for real movies; the integration tests synth 3s clips and
    /// override to 0.
    pub fn with_seek_seconds(mut self, seek: u32) -> Self {
        self.seek_seconds = seek;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the cached file path if present, else extracts via ffmpeg
    /// and caches before returning. Movie / Episode pick a frame at 30 s;
    /// Audio extracts embedded cover art (phase-3 territory but trivial
    /// when present).
    #[instrument(skip(self), fields(media.id = %id))]
    pub async fn primary(
        &self,
        id: u64,
        kind: MediaKind,
        source: &Path,
    ) -> Result<PathBuf, ImageCacheError> {
        let out_path = primary_path(&self.root, kind, id);
        if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
            return Ok(out_path);
        }
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = out_path.with_extension("jpg.tmp");
        self.extract(source, kind, &tmp_path).await?;
        tokio::fs::rename(&tmp_path, &out_path).await?;
        Ok(out_path)
    }

    async fn extract(
        &self,
        source: &Path,
        kind: MediaKind,
        out: &Path,
    ) -> Result<(), ImageCacheError> {
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        // Explicit `-f mjpeg` because the cache writes to a `.tmp`
        // suffix path — ffmpeg can't infer the muxer from the .jpg.tmp
        // extension and dies with "Unable to choose an output format".
        let seek = self.seek_seconds.to_string();
        let args: Vec<&str> = match kind {
            MediaKind::Movie | MediaKind::Episode => vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-ss",
                &seek,
                "-i",
                source_str,
                "-frames:v",
                "1",
                "-q:v",
                "3",
                "-vf",
                "scale=480:-1",
                "-f",
                "mjpeg",
                out_str,
            ],
            MediaKind::Audio => vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-i",
                source_str,
                "-map",
                "0:v?",
                "-frames:v",
                "1",
                "-q:v",
                "3",
                "-f",
                "mjpeg",
                out_str,
            ],
        };
        let output = Command::new(&self.ffmpeg_bin)
            .args(&args)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ImageCacheError::Ffmpeg(output.status.code(), stderr));
        }
        // ffmpeg with -map 0:v? exits 0 even when no video stream — verify
        // the file actually has bytes.
        let meta = tokio::fs::metadata(out).await?;
        if meta.len() == 0 {
            let _ = tokio::fs::remove_file(out).await;
            return Err(ImageCacheError::Ffmpeg(Some(0), "no image written".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn primary_path_layout_per_kind() {
        let root = PathBuf::from("/srv/cache");
        assert_eq!(
            primary_path(&root, MediaKind::Movie, 7),
            PathBuf::from("/srv/cache/movie/7.jpg")
        );
        assert_eq!(
            primary_path(&root, MediaKind::Audio, 12),
            PathBuf::from("/srv/cache/audio/12.jpg")
        );
    }

    #[tokio::test]
    async fn primary_returns_cached_path_without_spawning_ffmpeg() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path()).with_ffmpeg("/no/such/ffmpeg");
        // Pre-seed cache so primary() short-circuits.
        let p = primary_path(td.path(), MediaKind::Movie, 42);
        tokio::fs::create_dir_all(p.parent().unwrap()).await.unwrap();
        tokio::fs::write(&p, b"fake-jpeg").await.unwrap();
        let got = cache
            .primary(42, MediaKind::Movie, Path::new("/no/source"))
            .await
            .unwrap();
        assert_eq!(got, p);
    }

    #[tokio::test]
    async fn primary_propagates_spawn_failure_when_ffmpeg_missing() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path()).with_ffmpeg("/no/such/ffmpeg");
        let res = cache
            .primary(99, MediaKind::Movie, Path::new("/no/source"))
            .await;
        assert!(matches!(res, Err(ImageCacheError::Io(_))));
    }
}
