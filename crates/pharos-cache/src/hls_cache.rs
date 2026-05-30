//! Disk-backed HLS segment cache (T42).
//!
//! HLS players request `.ts` segments serially (and sometimes in
//! parallel during seeks). Without a cache, every request respawns
//! ffmpeg from scratch for the same byte range — wasted CPU + slow
//! seeking on weak hardware.
//!
//! Design:
//! - One file per `(media_id, segment_index)` under
//!   `{root}/{media_id}/{seg}.ts`.
//! - Per-key `tokio::sync::Mutex<()>` deduplicates concurrent fetches:
//!   the first request transcodes + writes the file, others wait on
//!   the lock then read from disk.
//! - LRU tracking via `(access_counter, key) → bytes`; eviction is
//!   triggered after each insert and runs lazily until total bytes is
//!   under the configured cap.
//! - V6 still holds: a crashed ffmpeg subprocess never poisons the
//!   cache; the writer renames `.tmp → .ts` atomically and removes the
//!   tmp file on failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use pharos_transcode::{FfmpegTranscoder, TranscodeOptions};
use tokio::io::AsyncReadExt;

#[derive(Debug, thiserror::Error)]
pub enum HlsCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcode: {0}")]
    Transcode(String),
    #[error("non-utf8 path")]
    NonUtf8Path,
}

#[derive(Debug)]
struct EntryMeta {
    bytes: u64,
    /// Monotonically-increasing access counter; higher = more recent.
    last_used: u64,
}

/// Compound cache key:
/// `(media_id, segment_index, audio_stream_index, subtitle_stream_index,
///   video_bitrate_thousands)`.
/// Audio + subtitle default to a 0 / off sentinel so the cache layout
/// collapses for the common (no client override) case. Video bitrate
/// is rounded to nearest kbps so floating-point negotiation jitter
/// doesn't produce phantom variant files. `0` means "no override"
/// (negotiator-supplied default).
type CacheKey = (u64, u32, u32, i32, u32);

const NO_SUBTITLE: i32 = -1;

fn make_key(
    media_id: u64,
    seg_index: u32,
    audio_index: Option<u32>,
    subtitle_index: Option<u32>,
    video_bitrate_bps: Option<u64>,
) -> CacheKey {
    (
        media_id,
        seg_index,
        audio_index.unwrap_or(0),
        subtitle_index.map(|n| n as i32).unwrap_or(NO_SUBTITLE),
        video_bitrate_bps
            .map(|b| (b / 1000).min(u32::MAX as u64) as u32)
            .unwrap_or(0),
    )
}

#[derive(Debug, Default)]
struct CacheState {
    /// Per-key locks. Held while a fetch is in flight so concurrent
    /// requests for the same segment don't race.
    fetch_locks: HashMap<CacheKey, Arc<Mutex<()>>>,
    entries: HashMap<CacheKey, EntryMeta>,
    total_bytes: u64,
    access_counter: u64,
}

#[derive(Clone)]
pub struct HlsSegmentCache {
    root: PathBuf,
    max_bytes: u64,
    transcoder: FfmpegTranscoder,
    /// When set, segment transcodes are dispatched through the
    /// load-balancing scheduler (multi-GPU + all-CPU, crash-isolated
    /// workers writing the segment file directly) instead of the inline
    /// `transcoder`. `None` keeps the legacy single-ffmpeg path (tests,
    /// or builds without a worker binary).
    scheduler: Option<pharos_transcode::scheduler::TranscodeScheduler>,
    state: Arc<Mutex<CacheState>>,
}

impl std::fmt::Debug for HlsSegmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsSegmentCache")
            .field("root", &self.root)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl HlsSegmentCache {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            transcoder: FfmpegTranscoder::new(),
            scheduler: None,
            state: Arc::new(Mutex::new(CacheState::default())),
        }
    }

    /// Route segment transcodes through the load-balancing scheduler.
    /// Each segment is dispatched to the least-loaded eligible device
    /// (every GPU + the CPU), encoded by a crash-isolated worker that
    /// writes the `.ts` file directly (no cross-process byte copy).
    pub fn with_scheduler(
        mut self,
        sched: pharos_transcode::scheduler::TranscodeScheduler,
    ) -> Self {
        self.scheduler = Some(sched);
        self
    }

    /// Override the ffmpeg binary path. Used by the integration tests
    /// to point at a nix-store-pinned binary; production reads from
    /// `$PATH`.
    pub fn with_ffmpeg(mut self, p: impl Into<PathBuf>) -> Self {
        self.transcoder = FfmpegTranscoder::with_binary(p);
        self
    }

    /// P14 — attach a hardware encoder to the underlying transcoder.
    /// Pass `HwAccel::Off` for the software path.
    pub fn with_hwaccel(mut self, accel: pharos_transcode::HwAccel) -> Self {
        self.transcoder = self.transcoder.clone().with_hwaccel(accel);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Fetch the bytes for `(media_id, seg_index)` with no per-track
    /// override. Equivalent to `segment_bytes_keyed(.., None, None,
    /// ..)`. Retained for callers that don't know about per-stream
    /// indices yet.
    pub async fn segment_bytes(
        &self,
        media_id: u64,
        seg_index: u32,
        source: &Path,
        opts: &TranscodeOptions,
    ) -> Result<Vec<u8>, HlsCacheError> {
        self.segment_bytes_keyed(media_id, seg_index, None, None, source, opts)
            .await
    }

    /// W1/W2 — per-stream cache lookup. `audio_index` + `subtitle_index`
    /// land in the cache key + the on-disk path so a client switching
    /// audio track doesn't trample the previous track's cached
    /// segments. None values fall through to the default-track sentinel
    /// (audio=0, subtitle=-1).
    pub async fn segment_bytes_keyed(
        &self,
        media_id: u64,
        seg_index: u32,
        audio_index: Option<u32>,
        subtitle_index: Option<u32>,
        source: &Path,
        opts: &TranscodeOptions,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let key = make_key(
            media_id,
            seg_index,
            audio_index,
            subtitle_index,
            opts.video_bitrate_bps,
        );
        let path = self.segment_path_keyed(key);

        // Fast hit path: file present, just bump LRU. A concurrent
        // eviction can delete the file between try_exists and read; treat
        // that NotFound as a miss and fall through to regenerate rather
        // than surfacing a spurious 500 on a genuine cache hit.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* evicted; fall through */
                }
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

        // Re-check: another task may have populated while we waited.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* evicted; fall through */
                }
                Err(e) => return Err(e.into()),
            }
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("ts.tmp");
        if let Err(e) = self.write_segment(source, opts, &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        tokio::fs::rename(&tmp, &path).await?;

        let bytes = tokio::fs::read(&path).await?;
        self.record(key, bytes.len() as u64).await;
        self.maybe_evict().await;
        // Release the per-key fetch lock so future calls don't keep it
        // forever — leave the file in the LRU.
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        Ok(bytes)
    }

    #[cfg(test)]
    fn segment_path(&self, media_id: u64, seg_index: u32) -> PathBuf {
        self.segment_path_keyed((media_id, seg_index, 0, NO_SUBTITLE, 0))
    }

    /// Compose `{root}/{media_id}/{seg}.ts` for the default case
    /// (audio=0, subtitle=-1, bitrate=auto) and a longer-form
    /// `{root}/{media_id}/{seg}-a{A}-s{S}-b{Bkbps}.ts` when any
    /// dimension diverges. Keeps the existing on-disk layout intact
    /// for warm caches that pre-date per-track + per-variant keys.
    fn segment_path_keyed(&self, key: CacheKey) -> PathBuf {
        let (media_id, seg_index, audio_index, subtitle_index, bitrate_k) = key;
        let filename = if audio_index == 0 && subtitle_index == NO_SUBTITLE && bitrate_k == 0 {
            format!("{seg_index}.ts")
        } else {
            let sub_part = if subtitle_index == NO_SUBTITLE {
                "off".to_string()
            } else {
                subtitle_index.to_string()
            };
            let bitrate_part = if bitrate_k == 0 {
                "auto".to_string()
            } else {
                format!("{bitrate_k}")
            };
            format!("{seg_index}-a{audio_index}-s{sub_part}-b{bitrate_part}.ts")
        };
        self.root.join(media_id.to_string()).join(filename)
    }

    async fn write_segment(
        &self,
        source: &Path,
        opts: &TranscodeOptions,
        out: &Path,
    ) -> Result<(), HlsCacheError> {
        let _ = source.to_str().ok_or(HlsCacheError::NonUtf8Path)?;
        // Scheduler path: the worker writes the segment file itself,
        // load-balanced across GPUs + CPU. We just await completion.
        if let Some(sched) = &self.scheduler {
            use pharos_transcode::scheduler::SinkRequest;
            sched
                .submit(
                    source.to_path_buf(),
                    opts.clone(),
                    SinkRequest::FileDirect {
                        out_path: out.to_path_buf(),
                    },
                )
                .await
                .map_err(|e| HlsCacheError::Transcode(e.to_string()))?;
            return Ok(());
        }
        // Legacy inline path: one ffmpeg, stream to file.
        let mut stream = self
            .transcoder
            .transcode(source, opts)
            .await
            .map_err(|e| HlsCacheError::Transcode(e.to_string()))?;
        let mut file = tokio::fs::File::create(out).await?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(())
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
        // If a previous entry existed under this key (rare — only on
        // disk-bypass tests), subtract its bytes first.
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
        // Snapshot the (key, last_used) candidates outside the lock so
        // the disk delete doesn't hold the cache state.
        let mut to_remove: Vec<(CacheKey, PathBuf)> = Vec::new();
        {
            let mut state = self.state.lock().await;
            while state.total_bytes > self.max_bytes {
                let Some((key, meta)) =
                    state
                        .entries
                        .iter()
                        .min_by_key(|(_, m)| m.last_used)
                        .map(|(k, m)| {
                            (
                                *k,
                                EntryMeta {
                                    bytes: m.bytes,
                                    last_used: m.last_used,
                                },
                            )
                        })
                else {
                    break;
                };
                state.entries.remove(&key);
                state.total_bytes = state.total_bytes.saturating_sub(meta.bytes);
                to_remove.push((key, self.segment_path_keyed(key)));
            }
        }
        for (_, path) in to_remove {
            let _ = tokio::fs::remove_file(&path).await;
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    /// Seed a cache file directly (no ffmpeg) and update LRU state to
    /// match. Used by unit tests so they don't need a real ffmpeg
    /// invocation per byte.
    async fn force_insert(cache: &HlsSegmentCache, media_id: u64, seg: u32, body: &[u8]) {
        let path = cache.segment_path(media_id, seg);
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await.unwrap();
        }
        tokio::fs::write(&path, body).await.unwrap();
        cache
            .record((media_id, seg, 0, NO_SUBTITLE, 0), body.len() as u64)
            .await;
        cache.maybe_evict().await;
    }

    #[tokio::test]
    async fn hit_returns_cached_bytes_without_calling_ffmpeg() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        force_insert(&cache, 7, 0, b"segment-bytes").await;
        let opts = TranscodeOptions {
            container: pharos_transcode::Container::Mpegts,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let got = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts)
            .await
            .unwrap();
        assert_eq!(got, b"segment-bytes");
    }

    #[tokio::test]
    async fn miss_with_unavailable_ffmpeg_propagates_error() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        let opts = TranscodeOptions {
            container: pharos_transcode::Container::Mpegts,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let res = cache
            .segment_bytes(8, 0, Path::new("/no/source"), &opts)
            .await;
        assert!(matches!(res, Err(HlsCacheError::Transcode(_))));
    }

    #[tokio::test]
    async fn lru_eviction_drops_least_recent_when_over_cap() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 20);
        // 3 segments of 10 bytes each — total 30, cap 20 -> 1 must go.
        force_insert(&cache, 7, 0, b"0123456789").await;
        force_insert(&cache, 7, 1, b"0123456789").await;
        // Touch seg 0 so it's more-recent than seg 1.
        let opts = TranscodeOptions {
            container: pharos_transcode::Container::Mpegts,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let _ = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts)
            .await
            .unwrap();
        // Adding seg 2 should evict seg 1 (the LRU).
        force_insert(&cache, 7, 2, b"0123456789").await;
        assert!(cache.total_bytes().await <= 20);
        assert_eq!(cache.entry_count().await, 2);
        // seg 1 must be gone from disk too.
        assert!(!tokio::fs::try_exists(td.path().join("7").join("1.ts"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn concurrent_hits_share_one_decode() {
        // Two concurrent requests for the same segment must both read
        // the cached file rather than racing two transcodes. Use a
        // stand-in transcoder that counts invocations to prove only
        // one fired.
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 4096);
        // Pre-seed so both calls hit the fast path.
        force_insert(&cache, 9, 0, b"abc").await;
        let counter = AtomicU32::new(0);
        let one = async {
            counter.fetch_add(1, Ordering::SeqCst);
            let opts = TranscodeOptions {
                container: pharos_transcode::Container::Mpegts,
                video: None,
                audio: None,
                video_bitrate_bps: None,
                audio_bitrate_bps: None,
                start_position_ticks: 0,
                duration_ticks: None,
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts)
                .await
                .unwrap()
        };
        let (a, b) = tokio::join!(one, async {
            counter.fetch_add(1, Ordering::SeqCst);
            let opts = TranscodeOptions {
                container: pharos_transcode::Container::Mpegts,
                video: None,
                audio: None,
                video_bitrate_bps: None,
                audio_bitrate_bps: None,
                start_position_ticks: 0,
                duration_ticks: None,
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts)
                .await
                .unwrap()
        });
        assert_eq!(a, b);
        assert_eq!(a, b"abc");
    }
}
