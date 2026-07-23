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
    /// P48 — optional in-process libav worker pool. When set (server
    /// `ffmpeg-lib` build), video-frame extraction routes through a
    /// resident worker instead of forking ffmpeg per thumbnail.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pool: Option<pharos_transcode::worker::LibavWorkerPool>,
    /// B72/T95 — per-key single-flight. A library grid fires many requests for
    /// the same missing poster at once; without this each ran its own ffmpeg
    /// extract AND raced on the shared `.jpg.tmp` scratch path. String keys
    /// namespace the three fill sites (`fetch:`/`scale:`/`attach:`).
    locks: std::sync::Arc<crate::single_flight::KeyedLocks<String>>,
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
    /// The source genuinely carries no extractable image for this role —
    /// e.g. an audio file with no embedded cover art. Distinct from a
    /// transient [`Ffmpeg`]/[`Io`] failure: it's a permanent "there is
    /// nothing here", so the caller negatively-caches it and never re-runs
    /// ffmpeg for the same key.
    ///
    /// [`Ffmpeg`]: ImageCacheError::Ffmpeg
    /// [`Io`]: ImageCacheError::Io
    #[error("source has no extractable image for this role")]
    NoContent,
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
pub fn image_path(root: &Path, role: ImageRole, kind: MediaKind, id: u64, index: u32) -> PathBuf {
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

/// Cache path for a Series *container*'s artwork (T9-series). A show has no
/// numeric MediaId (Series/Season are synthesised from their episodes), so its
/// poster/backdrop keys on a stable hash of the show's `series_key` and lives
/// under a dedicated `series` subdir — it can never collide with a real
/// movie/episode/audio id under the same role. The caller stores the returned
/// path in `series_metadata` and the images route serves it back verbatim, so
/// the hash only has to be self-consistent within one write (it is: a
/// fixed-seed `DefaultHasher`), not stable across toolchain versions.
pub fn series_image_path(root: &Path, role: ImageRole, series_key: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    series_key.hash(&mut h);
    let id = h.finish();
    root.join(role.as_dir())
        .join("series")
        .join(format!("{id}.jpg"))
}

/// Path of the negative-cache marker for a given image slot: an empty sentinel
/// written beside the (never-created) image when the source proved to have no
/// extractable content, so subsequent fetches skip the doomed ffmpeg run.
fn noart_sentinel(out_path: &Path) -> PathBuf {
    out_path.with_extension("noart")
}

/// A per-caller-unique scratch basename (V36). A fixed name lets two concurrent
/// batches collide on one temp dir; pid + a monotonic counter guarantee no two
/// calls in this process (or across replicas sharing the cache PVC) collide.
fn unique_scratch_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}.{}.{n}.tmp", std::process::id())
}

impl ImageCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            seek_seconds: 30,
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            pool: None,
            locks: std::sync::Arc::new(crate::single_flight::KeyedLocks::new()),
        }
    }

    pub fn with_ffmpeg(mut self, p: impl Into<PathBuf>) -> Self {
        self.ffmpeg_bin = p.into();
        self
    }

    /// Route video-frame extraction through the given resident libav
    /// worker pool (server `ffmpeg-lib` build). Audio cover-art extraction
    /// stays on the spawn path (it's an embedded-stream remux, not a
    /// decode-scale).
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pub fn with_pool(mut self, pool: pharos_transcode::worker::LibavWorkerPool) -> Self {
        self.pool = Some(pool);
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
        // B72/T95 — single-flight the miss: a library grid fires many requests
        // for the same missing poster at once. Without this each ran its own
        // ffmpeg extract AND wrote the SAME `.jpg.tmp` concurrently (corruption
        // before the atomic rename). Serialize per (id, role, index); the first
        // fills, the rest re-check and hit the warm file.
        let lock = self
            .locks
            .lock(format!("fetch:{id}:{role:?}:{index}"))
            .await;
        let _guard = lock.lock().await;
        if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
            return Ok(out_path);
        }
        // Negative cache: a prior extract proved this source has no image for
        // this role (e.g. a coverless audio file). Short-circuit to NoContent
        // without re-spawning ffmpeg — jellyfin-web re-requests these on every
        // library-grid render, and a storm of doomed extracts loads the libav
        // pool + spams the log.
        let noart_path = noart_sentinel(&out_path);
        if tokio::fs::try_exists(&noart_path).await.unwrap_or(false) {
            return Err(ImageCacheError::NoContent);
        }
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = out_path.with_extension("jpg.tmp");
        match self.extract(source, kind, role, &tmp_path).await {
            Ok(()) => {}
            Err(ImageCacheError::NoContent) => {
                // Record the "nothing here" verdict so the next request skips
                // ffmpeg entirely. Best-effort — a failed marker write just
                // means we retry the (cheap-to-fail) extract next time.
                let _ = tokio::fs::write(&noart_path, []).await;
                return Err(ImageCacheError::NoContent);
            }
            Err(e) => return Err(e),
        }
        tokio::fs::rename(&tmp_path, &out_path).await?;
        Ok(out_path)
    }

    /// Scale a local artwork file (a downloaded `poster.jpg` / `fanart.jpg`
    /// sidecar) down to `width` and cache the result, returning the cached
    /// path. Original sidecars are frequently multi-MB full-resolution art;
    /// serving them verbatim over NFS cost seconds per poster in a library
    /// grid, while jellyfin-web only ever displays a small thumbnail. The
    /// scaled JPEG is a few tens of KB → near-instant reads on repeat views.
    ///
    /// Cached under `{root}/scaled/{hash(path,mtime)}-w{width}.jpg`, keyed on
    /// the source path + mtime so re-downloaded art invalidates. Falls back to
    /// the original path on any scale failure (never fails a public image).
    pub async fn scaled_artwork(&self, source: &Path, width: u32) -> PathBuf {
        let Ok(meta) = tokio::fs::metadata(source).await else {
            return source.to_path_buf();
        };
        // Only large sidecars are worth scaling. A ~480px JPEG is tens of KB;
        // a full-res poster/fanart is hundreds of KB to several MB. Below this
        // threshold the file already reads fast and re-encoding would only add
        // latency + risk upscaling a small image, so serve it verbatim.
        const SCALE_THRESHOLD_BYTES: u64 = 256 * 1024;
        if meta.len() < SCALE_THRESHOLD_BYTES {
            return source.to_path_buf();
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut h);
        mtime.hash(&mut h);
        let key = h.finish();
        let dir = self.root.join("scaled");
        let out = dir.join(format!("{key:016x}-w{width}.jpg"));
        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            return out;
        }
        // Single-flight per (source, mtime, width): a grid re-uses the same
        // sidecar for every visible tile, so concurrent scalers otherwise raced
        // on one `.jpg.tmp` (T95/V36).
        let lock = self.locks.lock(format!("scale:{key:016x}:w{width}")).await;
        let _guard = lock.lock().await;
        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            return out;
        }
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return source.to_path_buf();
        }
        let tmp = out.with_extension("jpg.tmp");
        match self.scale_to(source, width, &tmp).await {
            Ok(()) => match tokio::fs::rename(&tmp, &out).await {
                Ok(()) => out,
                Err(_) => source.to_path_buf(),
            },
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                tracing::warn!(error = %e, source = %source.display(), "artwork scale failed; serving original");
                source.to_path_buf()
            }
        }
    }

    /// Decode an image file, scale it to `width` (aspect preserved), and write
    /// a JPEG to `out`. Uses the resident libav worker when available (prod),
    /// else forks ffmpeg — `extract_image` opens a still image the same way it
    /// opens a video (single frame), so this reuses that path.
    async fn scale_to(&self, source: &Path, width: u32, out: &Path) -> Result<(), ImageCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            return pool
                .extract_image(source.to_path_buf(), None, width, 3, out.to_path_buf())
                .await
                .map_err(|e| ImageCacheError::Ffmpeg(None, format!("libav scale: {e}")));
        }
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let scale = format!("scale={width}:-1");
        let status = Command::new(&self.ffmpeg_bin)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-i",
                source_str,
                "-vf",
                &scale,
                "-frames:v",
                "1",
                "-f",
                "mjpeg",
                "-q:v",
                "3",
                "-y",
                out_str,
            ])
            .status()
            .await
            .map_err(|e| ImageCacheError::Ffmpeg(None, format!("spawn: {e}")))?;
        if !status.success() {
            return Err(ImageCacheError::Ffmpeg(
                status.code(),
                "ffmpeg scale failed".into(),
            ));
        }
        Ok(())
    }

    /// Extract embedded attachment stream `stream_index` (a font) from
    /// `source` to a cached file, returning its path. Cached under
    /// `{root}/attachments/{id}/{index}`; extraction runs once on a miss.
    /// jellyfin-web fetches these for SubtitlesOctopus so ASS/SSA render.
    pub async fn attachment(
        &self,
        id: u64,
        source: &Path,
        stream_index: u32,
    ) -> Result<PathBuf, ImageCacheError> {
        let dir = self.root.join("attachments").join(id.to_string());
        let out = dir.join(stream_index.to_string());
        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            return Ok(out);
        }
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = out.with_extension("tmp");
        if let Err(e) = self.extract_attachment_to(source, stream_index, &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        tokio::fs::rename(&tmp, &out).await?;
        Ok(out)
    }

    /// Write attachment stream `stream_index`'s bytes to `out` — resident libav
    /// worker in prod, else `ffmpeg -dump_attachment`.
    async fn extract_attachment_to(
        &self,
        source: &Path,
        stream_index: u32,
        out: &Path,
    ) -> Result<(), ImageCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            return pool
                .extract_attachment(source.to_path_buf(), stream_index, out.to_path_buf())
                .await
                .map_err(|e| ImageCacheError::Ffmpeg(None, format!("libav attachment: {e}")));
        }
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        // `-dump_attachment:<idx> <file>` writes the attachment; the `-f null`
        // sink satisfies ffmpeg's "need an output" without producing one.
        let status = Command::new(&self.ffmpeg_bin)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                &format!("-dump_attachment:{stream_index}"),
                out_str,
                "-i",
                source_str,
                "-f",
                "null",
                "-",
            ])
            .status()
            .await
            .map_err(|e| ImageCacheError::Ffmpeg(None, format!("spawn: {e}")))?;
        if tokio::fs::try_exists(out).await.unwrap_or(false) {
            return Ok(());
        }
        Err(ImageCacheError::Ffmpeg(
            status.code(),
            "ffmpeg dump_attachment produced no file".into(),
        ))
    }

    /// Ensure EVERY listed attachment (font) is extracted, in a SINGLE source
    /// open, and return the directory holding `{stream_index}` files. ASS/SSA
    /// subtitles reference many fonts and SubtitlesOctopus fetches all of them
    /// before drawing a cue; extracting one-per-request re-opens the (NFS,
    /// multi-GB) source N times and stalls the "Fetching assets" phase. Warm
    /// them together so only the first font request pays one open and the rest
    /// hit cache. `indices` are the ffprobe attachment stream indices from the
    /// item probe.
    pub async fn ensure_all_attachments(
        &self,
        id: u64,
        source: &Path,
        indices: &[u32],
    ) -> Result<PathBuf, ImageCacheError> {
        let dir = self.root.join("attachments").join(id.to_string());
        // Fast path: all requested indices already resident.
        if !indices.is_empty() {
            let mut all = true;
            for &idx in indices {
                if !tokio::fs::try_exists(dir.join(idx.to_string()))
                    .await
                    .unwrap_or(false)
                {
                    all = false;
                    break;
                }
            }
            if all {
                return Ok(dir);
            }
        }
        // B72/T95 — single-flight per item: two concurrent warms of the same
        // item (e.g. the trickplay backfill and an on-demand font fetch) each
        // wiped + rebuilt the shared batch scratch dir, so one could delete the
        // other's in-flight dump. Serialize per id, then re-check the fast path.
        let lock = self.locks.lock(format!("attach:{id}")).await;
        let _guard = lock.lock().await;
        if !indices.is_empty() {
            let mut all = true;
            for &idx in indices {
                if !tokio::fs::try_exists(dir.join(idx.to_string()))
                    .await
                    .unwrap_or(false)
                {
                    all = false;
                    break;
                }
            }
            if all {
                return Ok(dir);
            }
        }
        tokio::fs::create_dir_all(&dir).await?;
        // Extract into a scratch dir in one pass, then move each file into
        // place (atomic per file) so a concurrent reader never sees a partial.
        // The scratch name is per-caller-unique (V36) as a second line of
        // defence: even without the lock, two batches never share a temp dir.
        let tmp = dir.join(unique_scratch_name(".batch"));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await?;
        // One cold source open dumps every font. Timed so the ASS "Fetching
        // assets" cost is visible: this is the whole first-font latency.
        let started = std::time::Instant::now();
        let res = self.extract_all_attachments_to(source, indices, &tmp).await;
        tracing::info!(
            media.id = id,
            fonts = indices.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            ok = res.is_ok(),
            "attachment batch extracted (one source open)"
        );
        if let Err(e) = res {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(e);
        }
        let mut rd = tokio::fs::read_dir(&tmp).await?;
        while let Some(entry) = rd.next_entry().await? {
            let dst = dir.join(entry.file_name());
            let _ = tokio::fs::rename(entry.path(), &dst).await;
        }
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        Ok(dir)
    }

    /// Dump all attachments to `out_dir` in one open — resident libav worker in
    /// prod (discovers attachment streams itself), else one `ffmpeg` process
    /// with a `-dump_attachment:<idx>` per index (still a single input open).
    async fn extract_all_attachments_to(
        &self,
        source: &Path,
        indices: &[u32],
        out_dir: &Path,
    ) -> Result<(), ImageCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            return pool
                .extract_all_attachments(source.to_path_buf(), out_dir.to_path_buf())
                .await
                .map(|_| ())
                .map_err(|e| {
                    ImageCacheError::Ffmpeg(None, format!("libav batch attachment: {e}"))
                });
        }
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out_dir.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        // One ffmpeg process, N `-dump_attachment` input options → one open.
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
            "-y".into(),
        ];
        for idx in indices {
            args.push(format!("-dump_attachment:{idx}"));
            args.push(format!("{out_str}/{idx}"));
        }
        args.push("-i".into());
        args.push(source_str.into());
        args.push("-f".into());
        args.push("null".into());
        args.push("-".into());
        let _ = Command::new(&self.ffmpeg_bin)
            .args(&args)
            .status()
            .await
            .map_err(|e| ImageCacheError::Ffmpeg(None, format!("spawn: {e}")))?;
        // ffmpeg exits non-zero after dumping attachments (the null sink has no
        // stream to mux), so don't gate on the status — verify a file landed.
        for idx in indices {
            if tokio::fs::try_exists(out_dir.join(idx.to_string()))
                .await
                .unwrap_or(false)
            {
                return Ok(());
            }
        }
        Err(ImageCacheError::Ffmpeg(
            None,
            "ffmpeg dump_attachment produced no files".into(),
        ))
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

    /// Atomically persist a Series *container*'s downloaded artwork bytes
    /// (T9-series), returning the cache path to record in `series_metadata`.
    /// Keyed by `series_key` under the dedicated `series` subdir (see
    /// [`series_image_path`]) so it never collides with a real item's image.
    #[instrument(skip(self, body), fields(series_key = %series_key, role = ?role, bytes = body.len()))]
    pub async fn upload_series_art(
        &self,
        series_key: &str,
        role: ImageRole,
        body: &[u8],
    ) -> Result<PathBuf, ImageCacheError> {
        let out_path = series_image_path(&self.root, role, series_key);
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

    /// P32 — chapter thumbnail extractor. Seeks to `start_ms`, emits
    /// a 480-px-wide JPEG cached at `{root}/{media_id}/chapter-{idx}.jpg`.
    /// Returns the path on hit; ffmpeg runs only on miss + writes
    /// atomically via the existing `.tmp → final` pattern.
    #[instrument(skip(self), fields(media.id = %id, idx = %chapter_idx, start_ms = %start_ms))]
    pub async fn chapter(
        &self,
        id: u64,
        source: &Path,
        chapter_idx: u32,
        start_ms: u64,
    ) -> Result<PathBuf, ImageCacheError> {
        let out_path = self
            .root
            .join(id.to_string())
            .join(format!("chapter-{chapter_idx}.jpg"));
        if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
            return Ok(out_path);
        }
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = out_path.with_extension("jpg.tmp");
        self.extract_chapter(source, start_ms, &tmp_path).await?;
        tokio::fs::rename(&tmp_path, &out_path).await?;
        Ok(out_path)
    }

    async fn extract_chapter(
        &self,
        source: &Path,
        start_ms: u64,
        out: &Path,
    ) -> Result<(), ImageCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            return pool
                .extract_image(
                    source.to_path_buf(),
                    Some(start_ms),
                    480,
                    3,
                    out.to_path_buf(),
                )
                .await
                .map_err(|e| ImageCacheError::Ffmpeg(None, format!("libav: {e}")));
        }
        let source_str = source.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let out_str = out.to_str().ok_or(ImageCacheError::NonUtf8Path)?;
        let seek = format!("{}", start_ms as f64 / 1000.0);
        let args: [&str; 17] = [
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
            // mjpeg requires a full-range (yuvj*) pixel format; the scale
            // filter emits limited-range yuv420p, which ffmpeg 8.1's mjpeg
            // encoder rejects ("Non full-range YUV is non-standard").
            "-pix_fmt",
            "yuvj420p",
        ];
        let output = Command::new(&self.ffmpeg_bin)
            .args(args)
            .arg("-f")
            .arg("mjpeg")
            .arg(out_str)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ImageCacheError::Ffmpeg(output.status.code(), stderr));
        }
        let meta = tokio::fs::metadata(out).await?;
        if meta.len() == 0 {
            let _ = tokio::fs::remove_file(out).await;
            return Err(ImageCacheError::Ffmpeg(Some(0), "no image written".into()));
        }
        Ok(())
    }

    /// Enforce a soft byte cap on the whole cache tree: recount every file,
    /// and if the total exceeds `max_bytes`, delete the oldest (by mtime)
    /// until back under. `max_bytes == 0` disables the cap (no-op) and returns
    /// 0. Returns the resulting total bytes.
    ///
    /// Unlike an in-memory LRU (which forgets pre-existing files across a
    /// restart), recounting the disk each pass stays correct over restarts and
    /// across every write path — fetch / scaled_artwork / attachment /
    /// chapter / upload — without instrumenting any of them. Eviction is by
    /// mtime (oldest-written first): true atime-LRU is unreliable under
    /// relatime/noatime, and an evicted image is cheap to re-extract on the
    /// next request (V6: a miss is never fatal). Intended to run periodically
    /// from a background janitor, not on the request path.
    pub async fn enforce_cap(&self, max_bytes: u64) -> u64 {
        if max_bytes == 0 {
            return 0;
        }
        struct Scan {
            path: PathBuf,
            size: u64,
            mtime: std::time::SystemTime,
        }
        let mut files: Vec<Scan> = Vec::new();
        let mut total: u64 = 0;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(entry.path());
                } else if meta.is_file() {
                    let size = meta.len();
                    total = total.saturating_add(size);
                    files.push(Scan {
                        path: entry.path(),
                        size,
                        mtime: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    });
                }
            }
        }
        if total <= max_bytes {
            return total;
        }
        // Oldest first — evict until under the cap.
        files.sort_by_key(|f| f.mtime);
        let mut evicted: u64 = 0;
        for f in files {
            if total <= max_bytes {
                break;
            }
            if tokio::fs::remove_file(&f.path).await.is_ok() {
                total = total.saturating_sub(f.size);
                evicted = evicted.saturating_add(f.size);
            }
        }
        tracing::info!(
            evicted_bytes = evicted,
            remaining_bytes = total,
            cap_bytes = max_bytes,
            "image cache: cap enforced"
        );
        total
    }

    async fn extract(
        &self,
        source: &Path,
        kind: MediaKind,
        role: ImageRole,
        out: &Path,
    ) -> Result<(), ImageCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            // Video-frame roles go through the resident worker. Audio
            // cover art (embedded attached-pic remux) stays on spawn.
            if matches!(kind, MediaKind::Movie | MediaKind::Episode) {
                let seek = match role {
                    ImageRole::Backdrop => {
                        self.seek_seconds.saturating_mul(4).max(self.seek_seconds)
                    }
                    _ => self.seek_seconds,
                };
                let width = match role {
                    ImageRole::Backdrop => 1280,
                    ImageRole::Thumb => 640,
                    _ => 480,
                };
                return pool
                    .extract_image(
                        source.to_path_buf(),
                        Some(seek as u64 * 1000),
                        width,
                        3,
                        out.to_path_buf(),
                    )
                    .await
                    .map_err(|e| ImageCacheError::Ffmpeg(None, format!("libav: {e}")));
            }
        }
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
                // Full-range pixel format the mjpeg encoder requires.
                "-pix_fmt",
                "yuvj420p",
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
                "-pix_fmt",
                "yuvj420p",
                "-f",
                "mjpeg",
                out_str,
            ],
        };
        let output = Command::new(&self.ffmpeg_bin).args(&args).output().await?;
        if !output.status.success() {
            // Audio frame-extract (`-map 0:v?`) only fails because the file has
            // no embedded cover-art stream ("Output file does not contain any
            // stream") — a permanent NoContent the caller negatively-caches, not
            // a transient error worth logging/retrying. Video failures stay
            // surfaced as Ffmpeg errors.
            if matches!(kind, MediaKind::Audio) {
                return Err(ImageCacheError::NoContent);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ImageCacheError::Ffmpeg(output.status.code(), stderr));
        }
        // ffmpeg with -map 0:v? exits 0 even when no video stream — verify
        // the file actually has bytes.
        let meta = tokio::fs::metadata(out).await?;
        if meta.len() == 0 {
            let _ = tokio::fs::remove_file(out).await;
            if matches!(kind, MediaKind::Audio) {
                return Err(ImageCacheError::NoContent);
            }
            return Err(ImageCacheError::Ffmpeg(Some(0), "no image written".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn small_sidecar_is_served_verbatim_not_scaled() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path().join("cache"));
        // A small sidecar (< 256 KiB) must be returned as-is: no re-encode,
        // no scaled copy — it already reads fast.
        let small = dir.path().join("poster.jpg");
        tokio::fs::write(&small, vec![0u8; 4096]).await.unwrap();
        let out = cache.scaled_artwork(&small, 480).await;
        assert_eq!(out, small, "small art must pass through unscaled");
    }

    #[tokio::test]
    async fn missing_source_returns_input_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path().join("cache"));
        let missing = dir.path().join("nope.jpg");
        assert_eq!(cache.scaled_artwork(&missing, 480).await, missing);
    }

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
        assert_eq!(
            ImageRole::from_str_ci("backdrop"),
            Some(ImageRole::Backdrop)
        );
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
    async fn noart_sentinel_short_circuits_without_spawning_ffmpeg() {
        let td = tempfile::TempDir::new().unwrap();
        // A bad ffmpeg bin: if fetch tried to extract, it'd fail with an Io
        // (spawn) error — distinct from NoContent. So NoContent proves the
        // sentinel short-circuited before any ffmpeg spawn.
        let cache = ImageCache::new(td.path()).with_ffmpeg("/no/such/ffmpeg");
        let out = image_path(td.path(), ImageRole::Primary, MediaKind::Audio, 7, 0);
        tokio::fs::create_dir_all(out.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&noart_sentinel(&out), []).await.unwrap();
        let res = cache
            .fetch(
                7,
                ImageRole::Primary,
                MediaKind::Audio,
                Path::new("/coverless.mp3"),
                0,
            )
            .await;
        assert!(
            matches!(res, Err(ImageCacheError::NoContent)),
            "sentinel must short-circuit to NoContent, got {res:?}"
        );
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
    async fn enforce_cap_zero_is_a_noop() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path());
        let dir = td.path().join("primary").join("movie");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("1.jpg"), vec![0u8; 1000])
            .await
            .unwrap();
        // 0 = unbounded: nothing is scanned or deleted.
        assert_eq!(cache.enforce_cap(0).await, 0);
        assert!(tokio::fs::try_exists(dir.join("1.jpg")).await.unwrap());
    }

    #[tokio::test]
    async fn enforce_cap_evicts_oldest_until_under_the_cap() {
        let td = tempfile::TempDir::new().unwrap();
        let cache = ImageCache::new(td.path());
        let dir = td.path().join("primary").join("movie");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // Three 1000-byte files, oldest → newest, with a gap so mtimes order.
        for name in ["old.jpg", "mid.jpg", "new.jpg"] {
            tokio::fs::write(dir.join(name), vec![0u8; 1000])
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        // Cap at 2000 bytes: must evict the single oldest file.
        let total = cache.enforce_cap(2000).await;
        assert!(total <= 2000, "should be under cap, got {total}");
        assert!(
            !tokio::fs::try_exists(dir.join("old.jpg")).await.unwrap(),
            "oldest file evicted first"
        );
        assert!(
            tokio::fs::try_exists(dir.join("new.jpg")).await.unwrap(),
            "newest file kept"
        );
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
        tokio::fs::create_dir_all(p.parent().unwrap())
            .await
            .unwrap();
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
