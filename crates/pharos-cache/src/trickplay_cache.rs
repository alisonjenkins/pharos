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

/// Bump whenever trickplay sprite GENERATION changes in a way that makes
/// previously-cached tiles wrong (a pixel-format / colour / layout fix). On
/// startup a mismatch between this and the on-disk `.gen_version` marker wipes
/// every cached tile so it regenerates with the current code — the cache is
/// otherwise keyed only by `(media_id, width)` and never re-derives a tile that
/// already exists, so a stale tile from an older build would persist forever
/// (observed: magenta-cast Code Geass sprites from a pre-fix build). Starts at
/// 1: since existing deployments have no marker, the first startup after this
/// lands treats them as stale and regenerates once.
const TRICKPLAY_GEN_VERSION: u32 = 1;

/// Name of the on-disk generation-version marker at the cache root.
const GEN_VERSION_MARKER: &str = ".gen_version";

/// B39 — per-(media,width) completion marker, written INSIDE the sprite dir
/// only after every sheet landed. Presence of tiles alone is NOT completeness:
/// the libav worker writes sheets straight into the final dir, so an
/// interrupted run (heavy-op timeout on a long movie, worker crash, pod
/// restart mid-deploy) leaves a partial set that used to pass the tile-0
/// "generated" probe forever — advertised trickplay, grey previews past the
/// truncation point (observed live: Avatar, tiles ending at 1h39m58s).
const COMPLETE_MARKER: &str = ".complete";

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
    /// In-memory mirror of "tile 0 exists" per (media, width) — kept so the
    /// hot DTO path can gate `BaseItemDto.Trickplay` on ACTUAL tile presence
    /// without a disk stat per item per request (B35: the DTO used to
    /// advertise trickplay for every video regardless of disk state, so
    /// clients rendered an empty preview box until tiles existed). Primed by
    /// a one-shot root walk at construction; updated on generation and
    /// eviction; self-heals via `is_generated`'s disk fallback.
    generated: Arc<dashmap::DashSet<CacheKey>>,
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
        let root = root.into();
        Self::reconcile_generation(&root);
        let generated = Arc::new(dashmap::DashSet::new());
        // One-shot prime: walk {root}/{media}/{width}/0.jpg. Sync std::fs at
        // construction (boot) — ~1 stat per cached width dir.
        if let Ok(medias) = std::fs::read_dir(&root) {
            for m in medias.flatten() {
                let Some(mid) = m.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
                    continue;
                };
                let Ok(widths) = std::fs::read_dir(m.path()) else {
                    continue;
                };
                for w in widths.flatten() {
                    let Some(wv) = w.file_name().to_str().and_then(|n| n.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    // B39 — only a COMPLETE set counts; legacy/partial dirs
                    // (no marker) are re-verified by the backfill against the
                    // item's expected sheet count.
                    if w.path().join(COMPLETE_MARKER).is_file() {
                        generated.insert((mid, wv));
                    }
                }
            }
        }
        Self {
            root,
            generated,
            max_bytes,
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            state: Arc::new(Mutex::new(CacheState::default())),
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            pool: None,
        }
    }

    /// Wipe every cached tile when the on-disk generation version doesn't match
    /// [`TRICKPLAY_GEN_VERSION`], so a generation fix regenerates all sprites
    /// instead of serving stale ones forever. Runs once at construction (sync —
    /// startup only). Best-effort: any fs error leaves the cache as-is rather
    /// than aborting server boot.
    fn reconcile_generation(root: &Path) {
        let marker = root.join(GEN_VERSION_MARKER);
        let on_disk = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if on_disk == Some(TRICKPLAY_GEN_VERSION) {
            return;
        }
        // Version changed (or a pre-versioning cache with tiles already on
        // disk): drop everything under the root except the marker itself.
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.file_name().and_then(|n| n.to_str()) == Some(GEN_VERSION_MARKER) {
                    continue;
                }
                let _ = if p.is_dir() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
            }
        }
        let _ = std::fs::create_dir_all(root);
        let _ = std::fs::write(&marker, TRICKPLAY_GEN_VERSION.to_string());
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
        // B39 — stamp completeness before advertising the set.
        let _ = tokio::fs::write(self.key_dir(key).join(COMPLETE_MARKER), b"").await;
        self.generated.insert(key);
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

    /// True when the sprite set for `(media_id, width)` is COMPLETE on disk
    /// (B39: the `.complete` marker, not mere tile presence — an interrupted
    /// generation leaves tiles without the marker and must be re-attempted).
    pub async fn is_generated(&self, media_id: u64, width: u32) -> bool {
        if self.generated.contains(&(media_id, width)) {
            return true;
        }
        let path = self.key_dir((media_id, width)).join(COMPLETE_MARKER);
        let on_disk = tokio::fs::try_exists(&path).await.unwrap_or(false);
        if on_disk {
            self.generated.insert((media_id, width));
        }
        on_disk
    }

    /// B39 — migration/self-heal for sets generated before the completion
    /// marker existed (or whose marker was lost): if the on-disk sheets are
    /// consistent with `expected_tile_count`, stamp the marker and count the
    /// set complete; otherwise report false so the caller regenerates.
    /// Tolerates ONE missing trailing sheet — VFR sources and rounded
    /// container durations legitimately produce one fewer sheet than the
    /// duration-derived estimate.
    pub async fn verify_and_mark_complete(
        &self,
        media_id: u64,
        width: u32,
        expected_tile_count: u32,
    ) -> bool {
        let key: CacheKey = (media_id, width);
        if self.generated.contains(&key) {
            return true;
        }
        let dir = self.key_dir(key);
        if !tokio::fs::try_exists(&dir.join("0.jpg"))
            .await
            .unwrap_or(false)
        {
            return false;
        }
        let mut produced: u32 = 0;
        for n in 0..expected_tile_count {
            if tokio::fs::try_exists(&dir.join(format!("{n}.jpg")))
                .await
                .unwrap_or(false)
            {
                produced = n + 1;
            } else {
                break;
            }
        }
        if produced + 1 >= expected_tile_count {
            let _ = tokio::fs::write(dir.join(COMPLETE_MARKER), b"").await;
            self.generated.insert(key);
            true
        } else {
            false
        }
    }

    /// The subset of `widths` with tiles actually on disk for `media_id` —
    /// the sync, in-memory gate the DTO builders use for
    /// `BaseItemDto.Trickplay` (B35).
    pub fn generated_widths(&self, media_id: u64, widths: &[u32]) -> Vec<u32> {
        widths
            .iter()
            .copied()
            .filter(|w| self.generated.contains(&(media_id, *w)))
            .collect()
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
        // B39 — a marker-less set that matches the expected sheet count is a
        // pre-marker (or marker-lost) COMPLETE set: stamp it instead of
        // re-decoding the whole file.
        if self
            .verify_and_mark_complete(media_id, layout.width, layout.tile_count)
            .await
        {
            let mut state = self.state.lock().await;
            state.fetch_locks.remove(&key);
            return Ok(false);
        }
        // Anything else on disk is a torn partial run (interrupted mid-write):
        // wipe it so stale sheets never mix with the fresh set.
        let dir = self.key_dir(key);
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        let bytes_written = self.generate(key, layout, source).await?;
        // B39 — completion marker: written ONLY here, after every sheet
        // landed. is_generated keys on this, so an interrupted run (timeout,
        // crash, deploy) is re-attempted instead of serving grey previews.
        let _ = tokio::fs::write(dir.join(COMPLETE_MARKER), b"").await;
        self.record(key, bytes_written).await;
        self.maybe_evict().await;
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        Ok(true)
    }

    /// Generate the sprite set for EVERY width in `layouts` from a SINGLE source
    /// decode (B72/T96/V36). The largest width is decoded from the source; each
    /// smaller width is DERIVED by downscaling the master's sheets — a plain
    /// image resize, EXACT because every width shares the same tile grid and
    /// sheet count (`thumb_count`/`tile_count` depend only on duration+interval;
    /// only the pixel size differs). This replaces a loop that re-read the whole
    /// multi-GB NFS source once per width (3× for the default 320/640/1280).
    /// Returns `true` if any width was (re)generated.
    pub async fn ensure_generated_all(
        &self,
        media_id: u64,
        layouts: &[Layout],
        source: &Path,
    ) -> Result<bool, TrickplayCacheError> {
        let mut sorted: Vec<Layout> = layouts.to_vec();
        sorted.sort_by_key(|l| std::cmp::Reverse(l.width));
        let Some((master, rest)) = sorted.split_first() else {
            return Ok(false);
        };
        // Master decodes from the source via the full single-width machinery
        // (lock, verify-complete, wipe-partial, generate, marker, record).
        let mut any = self.ensure_generated(media_id, *master, source).await?;
        // Smaller widths derive from the master's on-disk sheets — no source
        // re-decode. If the master isn't present (generate failed/declined),
        // skip: the per-width on-demand path (`tile_bytes`) still self-heals.
        if self.is_generated(media_id, master.width).await {
            for target in rest {
                any |= self
                    .ensure_generated_derived(media_id, *master, *target)
                    .await?;
            }
        }
        Ok(any)
    }

    /// Materialise `target`'s sprite set by downscaling `master`'s sheets (no
    /// source decode). Mirrors [`Self::ensure_generated`]'s dedup + completeness
    /// bookkeeping so an interrupted derive re-runs instead of serving a partial.
    async fn ensure_generated_derived(
        &self,
        media_id: u64,
        master: Layout,
        target: Layout,
    ) -> Result<bool, TrickplayCacheError> {
        let key: CacheKey = (media_id, target.width);
        if self.is_generated(media_id, target.width).await {
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
        if self.is_generated(media_id, target.width).await {
            return Ok(false);
        }
        // A marker-less but complete set (pre-marker / marker-lost) → just stamp.
        if self
            .verify_and_mark_complete(media_id, target.width, target.tile_count)
            .await
        {
            let mut state = self.state.lock().await;
            state.fetch_locks.remove(&key);
            return Ok(false);
        }
        let dir = self.key_dir(key);
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        let bytes_written = self.derive(media_id, master, target).await?;
        let _ = tokio::fs::write(dir.join(COMPLETE_MARKER), b"").await;
        self.record(key, bytes_written).await;
        self.maybe_evict().await;
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        Ok(true)
    }

    /// Downscale every `{n}.jpg` sheet of `master` into `target`'s width. The
    /// whole tiled sheet is scaled by `target.width / master.width`, so each of
    /// the 100 thumbnails in the grid scales to the target thumbnail size — the
    /// grid is preserved, no re-decode. Returns the total bytes written.
    async fn derive(
        &self,
        media_id: u64,
        master: Layout,
        target: Layout,
    ) -> Result<u64, TrickplayCacheError> {
        let master_dir = self.key_dir((media_id, master.width));
        let dir = self.key_dir((media_id, target.width));
        tokio::fs::create_dir_all(&dir).await?;
        // Sheet pixel width = per-thumbnail width × grid columns.
        let target_sheet_px = target.width.saturating_mul(TILE_GRID);
        let mut total: u64 = 0;
        let mut n: u32 = 0;
        loop {
            let src = master_dir.join(format!("{n}.jpg"));
            if !tokio::fs::try_exists(&src).await.unwrap_or(false) {
                break;
            }
            let out = dir.join(format!("{n}.jpg"));
            self.scale_sheet(&src, target_sheet_px, &out).await?;
            if let Ok(meta) = tokio::fs::metadata(&out).await {
                total = total.saturating_add(meta.len());
            }
            n += 1;
        }
        if n == 0 {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(TrickplayCacheError::UnknownDuration);
        }
        Ok(total)
    }

    /// Scale one already-tiled sprite sheet JPEG to `width_px` wide (aspect
    /// preserved), writing a fresh JPEG to `out`. Resident libav worker in prod,
    /// else a one-shot ffmpeg scale — either way it decodes a single still, not
    /// the source media.
    async fn scale_sheet(
        &self,
        src: &Path,
        width_px: u32,
        out: &Path,
    ) -> Result<(), TrickplayCacheError> {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        if let Some(pool) = &self.pool {
            return pool
                .extract_image(src.to_path_buf(), None, width_px, 5, out.to_path_buf())
                .await
                .map_err(|e| TrickplayCacheError::Ffmpeg(-1, format!("libav sheet scale: {e}")));
        }
        let status = Command::new(&self.ffmpeg_bin)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(src)
            .arg("-vf")
            .arg(format!("scale={width_px}:-2:flags=fast_bilinear"))
            // Full-range for mjpeg (image2), same as the master generate path.
            .arg("-pix_fmt")
            .arg("yuvj420p")
            .arg("-q:v")
            .arg("5")
            .arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| TrickplayCacheError::Spawn(e.to_string()))?;
        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr).into_owned();
            return Err(TrickplayCacheError::Ffmpeg(
                status.status.code().unwrap_or(-1),
                stderr,
            ));
        }
        Ok(())
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
                    layout.thumb_count,
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
        for (key, dir) in to_remove {
            self.generated.remove(&key);
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
    fn stale_tiles_wiped_when_gen_version_absent_then_marker_persists() {
        let td = TempDir::new().unwrap();
        // Simulate a pre-versioning cache: a tile on disk, no marker.
        let stale_dir = td.path().join("42").join("320");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("0.jpg"), b"stale").unwrap();

        // Constructing the cache reconciles: no marker → wipe + write marker.
        let _cache = TrickplayCache::new(td.path(), 1024);
        assert!(
            !stale_dir.join("0.jpg").exists(),
            "stale tile must be wiped when the generation version is absent"
        );
        let marker = td.path().join(GEN_VERSION_MARKER);
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            TRICKPLAY_GEN_VERSION.to_string(),
            "marker written with current generation version"
        );

        // A tile written under the current version survives a later construction
        // (marker matches → no wipe).
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("0.jpg"), b"fresh").unwrap();
        let _cache2 = TrickplayCache::new(td.path(), 1024);
        assert!(
            stale_dir.join("0.jpg").exists(),
            "tiles must persist when the generation version already matches"
        );
    }

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
    fn sheet_count_is_width_independent_so_derive_is_exact() {
        // B72/T96: ensure_generated_all decodes the largest width and downscales
        // the rest. That is EXACT only because every width shares the same tile
        // grid + sheet count (they depend on duration+interval, NOT width) — a
        // 1280 sheet scaled by 320/1280 is byte-for-byte the 320 sheet's layout.
        let dur = 37 * 60 * 1000; // 37 min
        let a = Layout::compute(dur, 1920, 1080, 1280, 10_000).unwrap();
        let b = Layout::compute(dur, 1920, 1080, 640, 10_000).unwrap();
        let c = Layout::compute(dur, 1920, 1080, 320, 10_000).unwrap();
        assert_eq!(a.thumb_count, b.thumb_count);
        assert_eq!(b.thumb_count, c.thumb_count);
        assert_eq!(a.tile_count, b.tile_count);
        assert_eq!(b.tile_count, c.tile_count);
        // Pixel dims DO differ — that's all the derive rescales.
        assert!(a.width > b.width && b.width > c.width);
        assert!(a.height > b.height && b.height > c.height);
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

        // Seed a tile. B39: a bare tile WITHOUT the completion marker is NOT
        // "generated" (it could be a truncated run) — but this layout expects
        // exactly one sheet, so ensure_generated's verify path recognises the
        // set as complete, stamps the marker, and no-ops (Ok(false)) instead
        // of re-decoding.
        let dir = cache.key_dir((11, 320));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let payload = b"\xFF\xD8\xFF\xE0fakejpeg";
        tokio::fs::write(dir.join("0.jpg"), payload).await.unwrap();
        assert!(
            !cache.is_generated(11, 320).await,
            "tiles without the completion marker must not count as generated (B39)"
        );
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
        assert!(
            cache.is_generated(11, 320).await,
            "verify path must stamp the marker for a complete legacy set"
        );
    }

    /// B39 — the Avatar failure: an interrupted generation leaves a TRUNCATED
    /// sheet set (no completion marker). It must not count as generated, and
    /// ensure_generated must wipe + re-attempt it rather than no-op.
    #[tokio::test]
    async fn truncated_sheet_set_is_regenerated_not_trusted() {
        let td = TempDir::new().unwrap();
        let cache = TrickplayCache::new(td.path(), 1 << 20).with_ffmpeg("/no/such/ffmpeg");
        // 3000s / 10s = 300 thumbs = 3 sheets expected.
        let layout = Layout::compute(3_000_000, 1920, 1080, 320, 10_000).unwrap();
        assert_eq!(layout.tile_count, 3);
        // Interrupted run: only sheet 0 landed.
        let dir = cache.key_dir((12, 320));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0.jpg"), b"partial")
            .await
            .unwrap();
        assert!(
            !cache.is_generated(12, 320).await,
            "truncated set must not count as generated"
        );
        assert!(
            !cache
                .verify_and_mark_complete(12, 320, layout.tile_count)
                .await,
            "verify must reject a truncated set"
        );
        // ensure_generated must try to REGENERATE (broken ffmpeg → error, which
        // proves it didn't trust the partial set).
        assert!(cache
            .ensure_generated(12, layout, std::path::Path::new("/n"))
            .await
            .is_err());

        // A set one sheet short of the estimate (VFR tolerance) IS complete.
        let dir = cache.key_dir((13, 320));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0.jpg"), b"s0").await.unwrap();
        tokio::fs::write(dir.join("1.jpg"), b"s1").await.unwrap();
        assert!(
            cache
                .verify_and_mark_complete(13, 320, layout.tile_count)
                .await,
            "one missing trailing sheet is within VFR tolerance"
        );
        assert!(cache.is_generated(13, 320).await);
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
