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
    #[error("image role requires upload")]
    UploadOnly,
}

/// Jellyfin's `ImageType` enum subset that pharos materialises on
/// disk. Primary is extracted from the source at the configured seek
/// time; Backdrop is extracted at the midpoint; the rest are
/// upload-only — they have no automatic extraction path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRole {
    Primary,
    Backdrop,
    Thumb,
    Logo,
    Banner,
    Art,
    Disc,
}

impl ImageRole {
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "primary" => Some(Self::Primary),
            "backdrop" => Some(Self::Backdrop),
            "thumb" => Some(Self::Thumb),
            "logo" => Some(Self::Logo),
            "banner" => Some(Self::Banner),
            "art" => Some(Self::Art),
            "disc" => Some(Self::Disc),
            _ => None,
        }
    }

    fn as_dir(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Backdrop => "backdrop",
            Self::Thumb => "thumb",
            Self::Logo => "logo",
            Self::Banner => "banner",
            Self::Art => "art",
            Self::Disc => "disc",
        }
    }

    /// Whether ffmpeg can synthesise this image from the source media.
    /// Logo / Banner / Art / Disc are upload-only — the source media
    /// itself doesn't carry them.
    fn is_extractable(self) -> bool {
        matches!(self, Self::Primary | Self::Backdrop | Self::Thumb)
    }
}

/// Where on disk a poster for the given media id lives. Default index
/// = 0; the Backdrop list is the only role that uses non-zero indices.
pub fn primary_path(root: &Path, kind: MediaKind, id: u64) -> PathBuf {
    image_path(root, ImageRole::Primary, kind, id, 0)
}

/// Per-role + per-index cache path. Index-0 forms the canonical "no
/// index" filename for any non-list role; jellyfin-web indexes
/// Backdrops at 0…N.
pub fn image_path(
    root: &Path,
    role: ImageRole,
    kind: MediaKind,
    id: u64,
    index: u32,
) -> PathBuf {
    let media = match kind {
        MediaKind::Movie => "movie",
        MediaKind::Episode => "episode",
        MediaKind::Audio => "audio",
    };
    let file = if index == 0 {
        format!("{id}.jpg")
    } else {
        format!("{id}-{index}.jpg")
    };
    root.join(role.as_dir()).join(media).join(file)
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
        self.fetch(id, ImageRole::Primary, kind, source, 0).await
    }

    /// Fetch any role + index. On extractable roles (Primary, Backdrop,
    /// Thumb) the file is materialised via ffmpeg if missing; on
    /// upload-only roles (Logo, Banner, Art, Disc) a missing file is
    /// reported as [`ImageCacheError::UploadOnly`].
    ///
    /// Backdrop seek timestamp scales with the source's duration —
    /// midpoint, clamped to the configured `seek_seconds` floor — so a
    /// blank intro doesn't dominate the image.
    #[instrument(skip(self), fields(media.id = %id, role = ?role, index = %index))]
    pub async fn fetch(
        &self,
        id: u64,
        role: ImageRole,
        kind: MediaKind,
        source: &Path,
        index: u32,
    ) -> Result<PathBuf, ImageCacheError> {
        let out_path = image_path(&self.root, role, kind, id, index);
        if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
            return Ok(out_path);
        }
        if !role.is_extractable() {
            return Err(ImageCacheError::UploadOnly);
        }
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = out_path.with_extension("jpg.tmp");
        self.extract(source, kind, role, &tmp_path).await?;
        tokio::fs::rename(&tmp_path, &out_path).await?;
        Ok(out_path)
    }

    /// Atomically persist a client-uploaded image at the given role +
    /// index slot. Used by `POST /Items/{id}/Images/{type}`.
    #[instrument(skip(self, body), fields(media.id = %id, role = ?role, bytes = body.len()))]
    pub async fn upload(
        &self,
        id: u64,
        role: ImageRole,
        kind: MediaKind,
        index: u32,
        body: &[u8],
    ) -> Result<PathBuf, ImageCacheError> {
        let out_path = image_path(&self.root, role, kind, id, index);
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = out_path.with_extension("jpg.tmp");
        tokio::fs::write(&tmp_path, body).await?;
        tokio::fs::rename(&tmp_path, &out_path).await?;
        Ok(out_path)
    }

    /// Remove the cached file for a given role + index. No-op when the
    /// file is already absent.
    #[instrument(skip(self), fields(media.id = %id, role = ?role, index = %index))]
    pub async fn remove(
        &self,
        id: u64,
        role: ImageRole,
        kind: MediaKind,
        index: u32,
    ) -> Result<(), ImageCacheError> {
        let out_path = image_path(&self.root, role, kind, id, index);
        match tokio::fs::remove_file(&out_path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn extract(
        &self,
        source: &Path,
        kind: MediaKind,
        role: ImageRole,
        out: &Path,
    ) -> Result<(), ImageCacheError> {
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        // Explicit `-f mjpeg` because the cache writes to a `.tmp`
        // suffix path — ffmpeg can't infer the muxer from the .jpg.tmp
        // extension and dies with "Unable to choose an output format".
        let seek = match role {
            // Backdrop sits deeper into the runtime than Primary so the
            // resulting frame is more visually distinct from the poster.
            // Falls back to `seek_seconds` on short fixtures.
            ImageRole::Backdrop => self.seek_seconds.saturating_mul(4).max(self.seek_seconds),
            _ => self.seek_seconds,
        };
        let seek = seek.to_string();
        let scale = match role {
            ImageRole::Backdrop => "scale=1280:-1",
            ImageRole::Thumb => "scale=640:-1",
            _ => "scale=480:-1",
        };
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
                scale,
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
        // T34: layout now `{role}/{kind}/{id}[-{index}].jpg`.
        assert_eq!(
            primary_path(&root, MediaKind::Movie, 7),
            PathBuf::from("/srv/cache/primary/movie/7.jpg")
        );
        assert_eq!(
            primary_path(&root, MediaKind::Audio, 12),
            PathBuf::from("/srv/cache/primary/audio/12.jpg")
        );
    }

    #[test]
    fn indexed_backdrop_path_uses_index_suffix() {
        let root = PathBuf::from("/srv/cache");
        assert_eq!(
            image_path(&root, ImageRole::Backdrop, MediaKind::Movie, 7, 0),
            PathBuf::from("/srv/cache/backdrop/movie/7.jpg"),
        );
        assert_eq!(
            image_path(&root, ImageRole::Backdrop, MediaKind::Movie, 7, 3),
            PathBuf::from("/srv/cache/backdrop/movie/7-3.jpg"),
        );
    }

    #[test]
    fn image_role_parses_case_insensitive() {
        assert_eq!(ImageRole::from_str_ci("Primary"), Some(ImageRole::Primary));
        assert_eq!(ImageRole::from_str_ci("backdrop"), Some(ImageRole::Backdrop));
        assert_eq!(ImageRole::from_str_ci("LOGO"), Some(ImageRole::Logo));
        assert_eq!(ImageRole::from_str_ci("nope"), None);
    }

    #[tokio::test]
    async fn upload_writes_atomically_then_fetch_hits_cache() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path()).with_ffmpeg("/no/such/ffmpeg");
        let body = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let path = cache
            .upload(42, ImageRole::Logo, MediaKind::Movie, 0, &body)
            .await
            .unwrap();
        let back = tokio::fs::read(&path).await.unwrap();
        assert_eq!(back, body);
        // Fetch on the same upload-only role now returns the path
        // (file is on disk; no ffmpeg required).
        let again = cache
            .fetch(42, ImageRole::Logo, MediaKind::Movie, Path::new("/n"), 0)
            .await
            .unwrap();
        assert_eq!(again, path);
    }

    #[tokio::test]
    async fn fetch_upload_only_role_without_file_errors_with_upload_only() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path()).with_ffmpeg("/no/such/ffmpeg");
        let res = cache
            .fetch(99, ImageRole::Logo, MediaKind::Movie, Path::new("/n"), 0)
            .await;
        assert!(matches!(res, Err(ImageCacheError::UploadOnly)));
    }

    #[tokio::test]
    async fn remove_is_idempotent_when_file_absent() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path());
        cache
            .remove(99, ImageRole::Primary, MediaKind::Movie, 0)
            .await
            .unwrap();
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
