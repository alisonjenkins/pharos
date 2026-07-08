//! Disk-backed Trickplay sprite cache.
//!
//! Generates a single sprite-grid set per `(media_id, width)` via one
//! ffmpeg call (`fps=1/interval, scale, tile=10x10`) and serves
//! individual tile JPEGs out of the resulting layout. Concurrency +
//! eviction follow the same pattern as `HlsSegmentCache`:
//!
//! - Per-key `tokio::sync::Mutex<()>` deduplicates concurrent fetches.
//! - LRU eviction keeps total bytes under the configured cap.
//! - `.tmp/` staging dir + atomic rename keeps the V6 invariant: a
//!   crashed ffmpeg never leaks a partial sprite into the served set.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum TrickplayCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg failed (exit {0}): {1}")]
    Ffmpeg(i32, String),
    #[error("ffmpeg spawn: {0}")]
    Spawn(String),
    #[error("source has no duration")]
    UnknownDuration,
    #[error("tile index {0} out of range (max {1})")]
    TileOutOfRange(u32, u32),
}

/// Per-cache layout knobs. Computed at the call site from probe data and config; the cache stores them so a later tile fetch can validate the requested index without re-deriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    /// Aspect-preserved render height. Always even (ffmpeg scaler).
    pub height: u32,
    pub interval_ms: u32,
    /// Total thumbnails across all tiles in this width.
    pub thumb_count: u32,
    /// Total `.jpg` tile files (each containing up to TILE_GRID²
    /// thumbnails).
    pub tile_count: u32,
}

/// Width × height of the sprite grid in one tile JPEG. Jellyfin
/// clients hard-code 10×10; do not change without breaking clients.
pub const TILE_GRID: u32 = 10;
const TILES_PER_FILE: u32 = TILE_GRID * TILE_GRID;

impl Layout {
    /// Compute layout from duration + source dimensions + the
    /// configured width + interval. Returns `None` when duration or
    /// source dimensions are missing (no sensible aspect ratio
    /// otherwise).
    pub fn compute(
        duration_ms: u64,
        src_width: u32,
        src_height: u32,
        target_width: u32,
        interval_ms: u32,
    ) -> Option<Self> {
        if duration_ms == 0 || src_width == 0 || src_height == 0 || interval_ms == 0 {
            return None;
        }
        let thumb_count = (duration_ms as u128)
            .div_ceil(interval_ms as u128)
            .min(u32::MAX as u128) as u32;
        if thumb_count == 0 {
            return None;
        }
        let tile_count = thumb_count.div_ceil(TILES_PER_FILE);
        let height = {
            let h = (target_width as u64 * src_height as u64 + (src_width as u64 / 2))
                / src_width as u64;
            // Even (ffmpeg's `-2` scale flag does the same).
            let h = (h / 2) * 2;
            h.max(2) as u32
        };
        Some(Layout {
            width: target_width,
            height,
            interval_ms,
            thumb_count,
            tile_count,
        })
    }
}

type CacheKey = (u64, u32);

#[derive(Debug)]
struct EntryMeta {
    bytes: u64,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    fetch_locks: HashMap<CacheKey, Arc<Mutex<()>>>,
    entries: HashMap<CacheKey, EntryMeta>,
    total_bytes: u64,
    access_counter: u64,
}

#[derive(Clone)]
pub struct TrickplayCache {
    root: PathBuf,
    max_bytes: u64,
    ffmpeg_bin: PathBuf,
    state: Arc<Mutex<CacheState>>,
    /// P48 — optional resident libav worker pool. When set, sprite-sheet
    /// generation runs in-process via a worker instead of forking ffmpeg.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pool: Option<pharos_transcode::worker::LibavWorkerPool>,
}

impl std::fmt::Debug for TrickplayCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrickplayCache")
            .field("root", &self.root)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl TrickplayCache {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            state: Arc::new(Mutex::new(CacheState::default())),
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            pool: None,
        }
    }

    pub fn with_ffmpeg(mut self, p: impl Into<PathBuf>) -> Self {
        self.ffmpeg_bin = p.into();
        self
    }

    /// Route sprite-sheet generation through the given resident libav
    /// worker pool (server `ffmpeg-lib` build).
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pub fn with_pool(mut self, pool: pharos_transcode::worker::LibavWorkerPool) -> Self {
        self.pool = Some(pool);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn key_dir(&self, key: CacheKey) -> PathBuf {
        let (media_id, width) = key;
        self.root.join(media_id.to_string()).join(width.to_string())
    }

    fn tile_path(&self, key: CacheKey, tile_index: u32) -> PathBuf {
        self.key_dir(key).join(format!("{tile_index}.jpg"))
    }

    /// Fetch the bytes for one tile JPEG. Generates the entire sprite
    /// set on first miss for `(media_id, width)`.
    pub async fn tile_bytes(
        &self,
        media_id: u64,
        layout: Layout,
        tile_index: u32,
        source: &Path,
    ) -> Result<Vec<u8>, TrickplayCacheError> {
        if tile_index >= layout.tile_count {
            return Err(TrickplayCacheError::TileOutOfRange(
                tile_index,
                layout.tile_count,
            ));
        }
        let key: CacheKey = (media_id, layout.width);
        let path = self.tile_path(key, tile_index);

        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            // Concurrent eviction may delete between try_exists and read;
            // treat NotFound as a miss and regenerate rather than 500.
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }

        let lock = {
            let mut state = self.state.lock().await;
            state
                .fetch_locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Re-check after acquiring the lock — peer task may have
        // generated the set while we waited.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            // Concurrent eviction may delete between try_exists and read;
            // treat NotFound as a miss and regenerate rather than 500.
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }

        let bytes_written = self.generate(key, layout, source).await?;
        self.record(key, bytes_written).await;
        self.maybe_evict().await;
        // Drop the per-key fetch lock — the LRU keeps the files.
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        drop(state);

        tokio::fs::read(&path).await.map_err(Into::into)
    }

    /// Fetch one tile's bytes ONLY if the sprite set is already cached — never
    /// generates. Returns `Ok(None)` on a miss so the HTTP handler can 404
    /// instantly instead of blocking a request on a minute-long whole-video
    /// generation (which also OOM-risked the process). Trickplay is populated
    /// out-of-band by the background pre-generator; the client simply shows no
    /// scrub preview until a sheet exists.
    pub async fn tile_bytes_cached(
        &self,
        media_id: u64,
        width: u32,
        tile_index: u32,
    ) -> Result<Option<Vec<u8>>, TrickplayCacheError> {
        let key: CacheKey = (media_id, width);
        let path = self.tile_path(key, tile_index);
        match tokio::fs::read(&path).await {
            Ok(b) => {
                self.touch(key).await;
                Ok(Some(b))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// True when tile 0 for `(media_id, width)` is already on disk — the cheap
    /// "already generated?" probe the background pre-generator uses to skip
    /// finished items without re-deriving the layout.
    pub async fn is_generated(&self, media_id: u64, width: u32) -> bool {
        let path = self.tile_path((media_id, width), 0);
        tokio::fs::try_exists(&path).await.unwrap_or(false)
    }

    /// Generate the full sprite set for `(media_id, width)` if it isn't already
    /// cached. Used by the background pre-generator so playback never triggers
    /// on-demand generation. Deduplicated + LRU-recorded exactly like
    /// [`Self::tile_bytes`]; a no-op (`Ok(false)`) when already present.
    pub async fn ensure_generated(
        &self,
        media_id: u64,
        layout: Layout,
        source: &Path,
    ) -> Result<bool, TrickplayCacheError> {
        let key: CacheKey = (media_id, layout.width);
        if self.is_generated(media_id, layout.width).await {
            return Ok(false);
        }
        let lock = {
            let mut state = self.state.lock().await;
            state
                .fetch_locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // Re-check under the lock — a peer may have generated while we waited.
        if self.is_generated(media_id, layout.width).await {
            return Ok(false);
        }
        let bytes_written = self.generate(key, layout, source).await?;
        self.record(key, bytes_written).await;
        self.maybe_evict().await;
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        Ok(true)
    }

    /// Run ffmpeg to populate every tile under `{root}/{media}/{width}/`.
    /// Stages into a sibling `.tmp/` dir and atomic-renames each file
    /// into place so a torn write never serves a partial sprite.
    async fn generate(
        &self,
        key: CacheKey,
        layout: Layout,
        source: &Path,
    ) -> Result<u64, TrickplayCacheError> {
        let dir = self.key_dir(key);
        tokio::fs::create_dir_all(&dir).await?;

        // P48 — resident-worker path: the libav helper writes 0-based
        // {i}.jpg sheets straight into `dir` (no tmp/rename dance), so we
        // just sum the produced bytes.
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            let produced = pool
                .trickplay(
                    source.to_path_buf(),
                    layout.interval_ms as u64,
                    layout.width,
                    TILE_GRID,
                    layout.tile_count,
                    5,
                    dir.clone(),
                )
                .await
                .map_err(|e| TrickplayCacheError::Ffmpeg(-1, format!("libav: {e}")))?;
            let mut total: u64 = 0;
            for n in 0..produced {
                if let Ok(meta) = tokio::fs::metadata(dir.join(format!("{n}.jpg"))).await {
                    total = total.saturating_add(meta.len());
                }
            }
            if total == 0 {
                return Err(TrickplayCacheError::UnknownDuration);
            }
            return Ok(total);
        }

        let tmp_dir = dir.with_extension("tmp");
        // Wipe any prior failed run.
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        tokio::fs::create_dir_all(&tmp_dir).await?;

        let interval_seconds = (layout.interval_ms as f64) / 1000.0;
        // `fps=1/N` — one frame per N seconds.
        // `scale=W:-2` — aspect-preserved, even height. flags=fast_bilinear
        //   keeps the encode cheap; sprite quality is low-stakes.
        // `tile=10x10` — pack into 10×10 grid per output.
        let vf = format!(
            "fps=1/{interval_seconds},scale={w}:-2:flags=fast_bilinear,tile={g}x{g}:padding=0:margin=0",
            interval_seconds = interval_seconds,
            w = layout.width,
            g = TILE_GRID,
        );

        // Output pattern — ffmpeg image2 muxer starts %d at 1; we
        // rename to 0-based after the run completes.
        let pattern = tmp_dir.join("%d.jpg");

        let mut cmd = Command::new(&self.ffmpeg_bin);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            // Keyframe-only decode (before -i): trickplay throws away >99% of
            // frames, so decoding only keyframes is an order of magnitude less
            // work; preview placement snaps to the nearest keyframe. Mirrors
            // the libav path's `skip_frame(NonKey)`.
            .arg("-skip_frame")
            .arg("nokey")
            .arg("-i")
            .arg(source)
            .arg("-vf")
            .arg(&vf)
            .arg("-an")
            .arg("-frames:v")
            .arg(layout.tile_count.to_string())
            .arg("-q:v")
            .arg("5")
            // Full-range pixel format for the mjpeg (image2) encoder; the
            // tile/scale filters emit limited-range yuv420p which ffmpeg
            // 8.1's mjpeg encoder rejects.
            .arg("-pix_fmt")
            .arg("yuvj420p")
            .arg("-f")
            .arg("image2")
            .arg(&pattern)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .map_err(|e| TrickplayCacheError::Spawn(e.to_string()))?;
        if !output.status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(TrickplayCacheError::Ffmpeg(
                output.status.code().unwrap_or(-1),
                stderr,
            ));
        }

        // Move ffmpeg's 1-based outputs into 0-based final names. The
        // real tile count depends on the decoded frame count, which
        // routinely differs from the duration-metadata estimate
        // (`layout.tile_count`) on VFR sources or with a rounded
        // container duration. Stop at the first missing tile (treat it
        // as end-of-output) instead of erroring — otherwise one missing
        // trailing tile discarded the entire successfully-generated
        // sprite set and 500'd every request for that item forever.
        let mut total: u64 = 0;
        let mut produced: u32 = 0;
        for n in 1..=layout.tile_count {
            let from = tmp_dir.join(format!("{n}.jpg"));
            let to = dir.join(format!("{}.jpg", n - 1));
            match tokio::fs::rename(&from, &to).await {
                Ok(()) => {
                    produced += 1;
                    if let Ok(meta) = tokio::fs::metadata(&to).await {
                        total = total.saturating_add(meta.len());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
                    return Err(TrickplayCacheError::Io(e));
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        if produced == 0 {
            // ffmpeg succeeded but emitted nothing usable — surface a
            // duration error rather than caching an empty set.
            return Err(TrickplayCacheError::UnknownDuration);
        }
        Ok(total)
    }

    async fn touch(&self, key: CacheKey) {
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        if let Some(meta) = state.entries.get_mut(&key) {
            meta.last_used = counter;
        }
    }

    async fn record(&self, key: CacheKey, bytes: u64) {
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        if let Some(old) = state.entries.insert(
            key,
            EntryMeta {
                bytes,
                last_used: counter,
            },
        ) {
            state.total_bytes = state.total_bytes.saturating_sub(old.bytes);
        }
        state.total_bytes = state.total_bytes.saturating_add(bytes);
    }

    async fn maybe_evict(&self) {
        let mut to_remove: Vec<(CacheKey, PathBuf)> = Vec::new();
        {
            let mut state = self.state.lock().await;
            while state.total_bytes > self.max_bytes {
                let Some((key, bytes)) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, m)| m.last_used)
                    .map(|(k, m)| (*k, m.bytes))
                else {
                    break;
                };
                state.entries.remove(&key);
                state.total_bytes = state.total_bytes.saturating_sub(bytes);
                to_remove.push((key, self.key_dir(key)));
            }
        }
        for (_, dir) in to_remove {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
    }

    #[cfg(test)]
    async fn total_bytes(&self) -> u64 {
        self.state.lock().await.total_bytes
    }

    #[cfg(test)]
    async fn entry_count(&self) -> usize {
        self.state.lock().await.entries.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn layout_compute_basic_320() {
        // 90s @ 10s interval → 9 thumbs → 1 tile.
        let l = Layout::compute(90_000, 1920, 1080, 320, 10_000).unwrap();
        assert_eq!(l.width, 320);
        // Aspect-preserved 320×180.
        assert_eq!(l.height, 180);
        assert_eq!(l.thumb_count, 9);
        assert_eq!(l.tile_count, 1);
    }

    #[test]
    fn layout_compute_wraps_into_multiple_tiles() {
        // 20 min @ 10s = 120 thumbs → 2 tiles (100 + 20).
        let l = Layout::compute(20 * 60 * 1000, 1920, 1080, 320, 10_000).unwrap();
        assert_eq!(l.thumb_count, 120);
        assert_eq!(l.tile_count, 2);
    }

    #[test]
    fn layout_compute_returns_none_on_zero_dims() {
        assert!(Layout::compute(0, 1920, 1080, 320, 10_000).is_none());
        assert!(Layout::compute(10_000, 0, 1080, 320, 10_000).is_none());
        assert!(Layout::compute(10_000, 1920, 0, 320, 10_000).is_none());
        assert!(Layout::compute(10_000, 1920, 1080, 320, 0).is_none());
    }

    #[test]
    fn layout_even_height_for_odd_aspect() {
        // 320:135 ≈ 21:9 ultrawide. Odd height should round down to even.
        let l = Layout::compute(60_000, 2560, 1080, 320, 10_000).unwrap();
        assert_eq!(l.height % 2, 0);
    }

    #[tokio::test]
    async fn out_of_range_tile_errors_without_running_ffmpeg() {
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        let layout = Layout::compute(90_000, 1920, 1080, 320, 10_000).unwrap();
        let res = cache
            .tile_bytes(7, layout, 99, std::path::Path::new("/no/source"))
            .await;
        assert!(matches!(
            res,
            Err(TrickplayCacheError::TileOutOfRange(_, _))
        ));
    }

    #[tokio::test]
    async fn miss_with_unavailable_ffmpeg_propagates_error() {
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        let layout = Layout::compute(90_000, 1920, 1080, 320, 10_000).unwrap();
        let res = cache
            .tile_bytes(8, layout, 0, std::path::Path::new("/no/source"))
            .await;
        assert!(matches!(res, Err(TrickplayCacheError::Spawn(_))));
    }

    #[tokio::test]
    async fn hit_returns_cached_bytes_without_spawning_ffmpeg() {
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        let layout = Layout::compute(90_000, 1920, 1080, 320, 10_000).unwrap();
        // Pre-seed the file + LRU.
        let dir = cache.key_dir((9, 320));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let payload = b"\xFF\xD8\xFF\xE0fakejpeg";
        tokio::fs::write(dir.join("0.jpg"), payload).await.unwrap();
        cache.record((9, 320), payload.len() as u64).await;
        let got = cache
            .tile_bytes(9, layout, 0, std::path::Path::new("/n"))
            .await
            .unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn cached_only_fetch_never_generates() {
        // The request-path helper must serve a cached tile but return None (not
        // generate) on a miss — with a bogus ffmpeg so any generation attempt
        // would surface as an error rather than silently succeeding.
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");

        // Miss → None, and no sprite dir gets created.
        assert!(!cache.is_generated(11, 320).await);
        assert_eq!(cache.tile_bytes_cached(11, 320, 0).await.unwrap(), None);
        let layout = Layout::compute(90_000, 1920, 1080, 320, 10_000).unwrap();
        // ensure_generated with a broken ffmpeg + real miss must error (proving
        // it actually tried to generate), not silently no-op.
        assert!(cache
            .ensure_generated(11, layout, std::path::Path::new("/n"))
            .await
            .is_err());

        // Seed a tile → hit returns it, and is_generated / ensure_generated
        // both see it as present (ensure_generated is a no-op → Ok(false)).
        let dir = cache.key_dir((11, 320));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let payload = b"\xFF\xD8\xFF\xE0fakejpeg";
        tokio::fs::write(dir.join("0.jpg"), payload).await.unwrap();
        assert!(cache.is_generated(11, 320).await);
        assert_eq!(
            cache
                .tile_bytes_cached(11, 320, 0)
                .await
                .unwrap()
                .as_deref(),
            Some(&payload[..])
        );
        assert!(!cache
            .ensure_generated(11, layout, std::path::Path::new("/n"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn lru_eviction_drops_least_recent_when_over_cap() {
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 20);
        // 3 sets of 10 bytes each — cap 20 → one must evict.
        for media_id in [10u64, 11, 12] {
            let dir = cache.key_dir((media_id, 320));
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(dir.join("0.jpg"), b"0123456789")
                .await
                .unwrap();
            cache.record((media_id, 320), 10).await;
            cache.maybe_evict().await;
        }
        assert!(cache.total_bytes().await <= 20);
        assert_eq!(cache.entry_count().await, 2);
        // Earliest (media 10) must be gone from disk.
        assert!(!tokio::fs::try_exists(td.path().join("10").join("320"))
            .await
            .unwrap());
    }
}
