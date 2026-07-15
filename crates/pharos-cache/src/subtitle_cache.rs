//! In-process subtitle cache.
//!
//! P5 — `subtitles.rs::deliver_vtt` (embedded WebVTT extraction) and
//! `serve_sidecar` (SRT → WebVTT conversion) both call ffmpeg on
//! every request. Subtitles are tiny and deterministic per
//! `(source_path, mtime, stream_index, kind)`; cache the converted
//! bytes so the second + Nth fetch never respawns ffmpeg.
//!
//! Concurrency follows the `HlsSegmentCache` pattern: per-key fetch
//! lock deduplicates concurrent first-fetches; LRU eviction keeps the
//! in-memory hot layer under the configured cap.
//!
//! **On-disk persistence (optional, via [`SubtitleCache::with_disk`]).**
//! Extracting an embedded subtitle demuxes the WHOLE source — a sparse
//! subtitle stream spans the entire container — so over an NFS-backed
//! multi-GB library a cold extraction costs tens of seconds, not the
//! "negligible" it was once assumed to be. A disk layer under the cache
//! PVC makes extraction a once-ever cost that survives pod restarts; the
//! in-memory map stays as a hot layer in front of it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubtitleKind {
    /// Embedded stream extracted via `ffmpeg -map 0:<idx> -f webvtt`.
    Embedded,
    /// External `.srt` sidecar converted to WebVTT.
    Sidecar,
    /// Embedded ASS/SSA stream extracted verbatim (`-c:s ass -f ass`) for
    /// SubtitlesOctopus, which needs the raw ASS body — distinct cache key
    /// from `Embedded` (same index, different bytes).
    EmbeddedAss,
    /// Embedded stream extracted as SubRip (`-c:s subrip -f srt`) for the
    /// legacy `Stream.srt` delivery form — distinct bytes/key from the WebVTT
    /// `Embedded` extraction of the same track.
    EmbeddedSrt,
    /// JSON `[(start_ms, end_ms), …]` on-screen event windows of an IMAGE
    /// subtitle stream (codec-relative index) — drives per-segment burn
    /// gating: segments with no event skip the overlay re-encode entirely.
    /// Computed once per (file, mtime, track) by a whole-file demux scan.
    EventWindows,
}

/// Cache key: source file + mtime stamps the input so any later edit
/// invalidates the cached bytes; stream index + kind distinguish
/// concurrent extractions from the same source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    mtime_secs: i64,
    stream_index: u32,
    kind: SubtitleKind,
}

#[derive(Debug)]
struct EntryMeta {
    bytes: u64,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    fetch_locks: HashMap<Key, Arc<Mutex<()>>>,
    entries: HashMap<Key, (Arc<Vec<u8>>, EntryMeta)>,
    total_bytes: u64,
    access_counter: u64,
}

/// Where a track's event-window scan stands. Callers that see anything but
/// `Known` burn conservatively (the pre-gating behaviour), so a missing /
/// failed / still-running scan can never LOSE subtitles — gating only ever
/// removes provably-empty burn windows.
#[derive(Debug, Clone)]
pub enum EventWindows {
    /// Merged, sorted `(start_ms, end_ms)` on-screen intervals.
    Known(Arc<Vec<(u64, u64)>>),
    /// Not computed yet — a background scan was kicked off (first ask) or
    /// is in flight; or the build has no libav pool. Burn conservatively.
    Unknown,
}

/// In-flight / negative state of a window scan, keyed per (path, mtime,
/// track). Separate from the byte LRU: entries are tiny and must not be
/// evicted mid-scan.
#[derive(Debug, Clone)]
enum WinScan {
    Pending,
    /// Scan failed this boot (bad stream, worker error). Don't re-scan on
    /// every segment; burn conservatively for the rest of the run.
    Failed,
}

#[derive(Clone)]
pub struct SubtitleCache {
    max_bytes: u64,
    max_entries: usize,
    /// Persistence root under the cache PVC. When set, extractions land on
    /// disk (`{root}/{key}.sub`) and survive restarts; the in-memory map is a
    /// hot layer in front. `None` → memory-only (tests / minimal deployments).
    root: Option<PathBuf>,
    state: Arc<Mutex<CacheState>>,
    /// Event-window scan bookkeeping (burn gating).
    win_scans: Arc<Mutex<HashMap<Key, WinScan>>>,
    /// Worker pool for the whole-file window scan (`ffmpeg-lib` builds; the
    /// distroless image ships no ffprobe, so the scan must be in-process).
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pool: Option<pharos_transcode::worker::LibavWorkerPool>,
    /// Adaptive background-I/O gate (the server's shared semaphore, squeezed
    /// to a trickle while a client streams). The window scan reads the WHOLE
    /// source file over NFS — ungated it fights the very stream that
    /// triggered it (observed live: burn segments ballooned 7-11 s → 46-56 s
    /// while the scan crawled the file being played).
    bg_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl std::fmt::Debug for SubtitleCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtitleCache")
            .field("max_bytes", &self.max_bytes)
            .field("max_entries", &self.max_entries)
            .finish()
    }
}

impl SubtitleCache {
    pub fn new(max_bytes: u64, max_entries: usize) -> Self {
        Self {
            max_bytes,
            max_entries,
            root: None,
            state: Arc::new(Mutex::new(CacheState::default())),
            win_scans: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            pool: None,
            bg_gate: None,
        }
    }

    /// Attach the shared adaptive bg-I/O semaphore — every window scan takes
    /// one permit for its whole run, so the whole-file NFS read yields to
    /// live playback instead of starving it.
    pub fn with_bg_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.bg_gate = Some(gate);
        self
    }

    /// Persist extracted subtitles under `root` (the cache PVC) so the cost is
    /// paid once ever and survives restarts, not re-incurred on every boot.
    pub fn with_disk(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Attach the shared libav worker pool — enables event-window scans
    /// (burn gating). Without it every ask returns
    /// [`EventWindows::Unknown`] and segments burn as before.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pub fn with_pool(mut self, pool: pharos_transcode::worker::LibavWorkerPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// On-screen event windows of the `rel_idx`-th subtitle stream (codec-
    /// relative, `-map 0:s:N` convention). Non-blocking: a cache hit (memory
    /// or disk) returns `Known`; the first miss kicks off ONE background
    /// whole-file scan and returns `Unknown` — callers burn conservatively
    /// until the scan lands (typically well within the first minutes of a
    /// watch; persisted per (path, mtime, track) so it's a once-ever cost).
    pub async fn image_sub_event_windows(
        &self,
        path: &Path,
        mtime_secs: i64,
        rel_idx: u32,
    ) -> EventWindows {
        if let Some(bytes) = self
            .get(path, mtime_secs, rel_idx, SubtitleKind::EventWindows)
            .await
        {
            match serde_json::from_slice::<Vec<(u64, u64)>>(&bytes) {
                Ok(w) => return EventWindows::Known(Arc::new(w)),
                Err(e) => {
                    // Corrupt persisted JSON — treat as absent and rescan.
                    tracing::warn!(error = %e, path = %path.display(), rel_idx,
                        "corrupt subtitle event-window cache entry; rescanning");
                }
            }
        }
        let key = Key {
            path: path.to_path_buf(),
            mtime_secs,
            stream_index: rel_idx,
            kind: SubtitleKind::EventWindows,
        };
        {
            let mut scans = self.win_scans.lock().await;
            match scans.get(&key) {
                Some(WinScan::Pending) | Some(WinScan::Failed) => return EventWindows::Unknown,
                None => {
                    scans.insert(key.clone(), WinScan::Pending);
                }
            }
        }
        self.spawn_window_scan(key);
        EventWindows::Unknown
    }

    /// Kick off the background scan for `key`. Split out so the non-libav
    /// build can stub it (no pool → mark Failed so we don't re-ask per
    /// segment).
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    fn spawn_window_scan(&self, key: Key) {
        let Some(pool) = self.pool.clone() else {
            let scans = self.win_scans.clone();
            tokio::spawn(async move {
                scans.lock().await.insert(key, WinScan::Failed);
            });
            return;
        };
        let cache = self.clone();
        let gate = self.bg_gate.clone();
        tokio::spawn(async move {
            // Whole-file NFS read → background-I/O gated (yields to playback).
            let _permit = match &gate {
                Some(g) => g.clone().acquire_owned().await.ok(),
                None => None,
            };
            let started = std::time::Instant::now();
            let res = pool
                .subtitle_windows(key.path.clone(), key.stream_index)
                .await;
            match res {
                Ok(windows) => {
                    let n = windows.len();
                    let json = match serde_json::to_vec(&windows) {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::warn!(error = %e, "event-window serialize failed");
                            cache.win_scans.lock().await.insert(key, WinScan::Failed);
                            return;
                        }
                    };
                    cache
                        .store(
                            &key.path,
                            key.mtime_secs,
                            key.stream_index,
                            SubtitleKind::EventWindows,
                            json,
                        )
                        .await;
                    cache.win_scans.lock().await.remove(&key);
                    tracing::info!(
                        path = %key.path.display(),
                        rel_idx = key.stream_index,
                        windows = n,
                        scan_ms = started.elapsed().as_millis() as u64,
                        "subtitle event-window scan complete; burn gating active"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %key.path.display(),
                        rel_idx = key.stream_index,
                        scan_ms = started.elapsed().as_millis() as u64,
                        "subtitle event-window scan failed; burning every segment for this track"
                    );
                    cache.win_scans.lock().await.insert(key, WinScan::Failed);
                }
            }
        });
    }

    /// No pool in this build — mark the key Failed once so callers stop
    /// re-asking; segments burn as before gating existed.
    #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
    fn spawn_window_scan(&self, key: Key) {
        let scans = self.win_scans.clone();
        tokio::spawn(async move {
            scans.lock().await.insert(key, WinScan::Failed);
        });
    }

    /// On-disk path for a key: `{root}/subtitles/{hash(path)}-{mtime}-{idx}-{k}.sub`.
    /// The source path is hashed (it contains `/` and arbitrary chars); mtime +
    /// index + kind keep distinct extractions apart and invalidate on edit.
    fn disk_path(&self, key: &Key) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.path.hash(&mut h);
        let ph = h.finish();
        let k = match key.kind {
            SubtitleKind::Embedded => 'e',
            SubtitleKind::Sidecar => 's',
            SubtitleKind::EmbeddedAss => 'a',
            SubtitleKind::EmbeddedSrt => 'r',
            SubtitleKind::EventWindows => 'w',
        };
        Some(root.join("subtitles").join(format!(
            "{ph:016x}-{}-{}-{k}.sub",
            key.mtime_secs, key.stream_index
        )))
    }

    /// Lookup the cached WebVTT bytes for this key. Returns `None` on
    /// miss; caller is expected to populate via `store`.
    pub async fn get(
        &self,
        path: &std::path::Path,
        mtime_secs: i64,
        stream_index: u32,
        kind: SubtitleKind,
    ) -> Option<Arc<Vec<u8>>> {
        let key = Key {
            path: path.to_path_buf(),
            mtime_secs,
            stream_index,
            kind,
        };
        {
            let mut state = self.state.lock().await;
            state.access_counter += 1;
            let counter = state.access_counter;
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.1.last_used = counter;
                return Some(entry.0.clone());
            }
        }
        // Memory miss → try the persistent disk layer (survives restart). A hit
        // promotes the bytes back into the in-memory hot map.
        let disk = self.disk_path(&key)?;
        let bytes = tokio::fs::read(&disk).await.ok()?;
        Some(self.insert(key, bytes).await)
    }

    /// Insert bytes into the in-memory hot map (shared by `get`'s disk-promote
    /// and `store`), returning the shared handle. Does NOT touch disk.
    async fn insert(&self, key: Key, bytes: Vec<u8>) -> Arc<Vec<u8>> {
        let len = bytes.len() as u64;
        let shared = Arc::new(bytes);
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        if let Some((_, old_meta)) = state.entries.insert(
            key,
            (
                shared.clone(),
                EntryMeta {
                    bytes: len,
                    last_used: counter,
                },
            ),
        ) {
            state.total_bytes = state.total_bytes.saturating_sub(old_meta.bytes);
        }
        state.total_bytes = state.total_bytes.saturating_add(len);
        self.evict_if_needed(&mut state);
        shared
    }

    /// Acquire the per-key fetch lock so concurrent first-fetchers
    /// don't all spawn ffmpeg. Caller is expected to invoke `get`
    /// again after acquiring the guard (peer may have populated while
    /// we waited).
    pub async fn lock(
        &self,
        path: &std::path::Path,
        mtime_secs: i64,
        stream_index: u32,
        kind: SubtitleKind,
    ) -> Arc<Mutex<()>> {
        let key = Key {
            path: path.to_path_buf(),
            mtime_secs,
            stream_index,
            kind,
        };
        let mut state = self.state.lock().await;
        state
            .fetch_locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Insert the resolved WebVTT bytes for this key. Triggers LRU
    /// eviction if the new total exceeds either bound.
    pub async fn store(
        &self,
        path: &std::path::Path,
        mtime_secs: i64,
        stream_index: u32,
        kind: SubtitleKind,
        bytes: Vec<u8>,
    ) -> Arc<Vec<u8>> {
        let key = Key {
            path: path.to_path_buf(),
            mtime_secs,
            stream_index,
            kind,
        };
        // Persist to disk first (atomic write) so a restart keeps the bytes; a
        // disk-write failure is non-fatal (memory layer still serves this run).
        if let Some(disk) = self.disk_path(&key) {
            if let Err(e) = write_atomic(&disk, &bytes).await {
                tracing::warn!(error = %e, path = %disk.display(), "subtitle cache disk write failed");
            }
        }
        let shared = self.insert(key.clone(), bytes).await;
        // Release the fetch lock — populated entry stays in the LRU.
        self.state.lock().await.fetch_locks.remove(&key);
        shared
    }

    fn evict_if_needed(&self, state: &mut CacheState) {
        while state.total_bytes > self.max_bytes || state.entries.len() > self.max_entries {
            let Some(victim) = state
                .entries
                .iter()
                .min_by_key(|(_, (_, m))| m.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, m)) = state.entries.remove(&victim) {
                state.total_bytes = state.total_bytes.saturating_sub(m.bytes);
            }
        }
    }

    #[cfg(test)]
    pub async fn entry_count(&self) -> usize {
        self.state.lock().await.entries.len()
    }

    #[cfg(test)]
    pub async fn total_bytes(&self) -> u64 {
        self.state.lock().await.total_bytes
    }
}

/// Write `bytes` to `path` atomically (temp + rename) so a concurrent reader
/// or a crash mid-write never sees a truncated subtitle. Creates the parent.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("sub.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Read the source path's mtime as seconds since epoch. Returns `0`
/// when the file is missing or stat fails — handlers fall through to
/// "always miss" behaviour, which is the conservative default.
pub async fn mtime_secs(path: &std::path::Path) -> i64 {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return 0;
    };
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn miss_returns_none() {
        let cache = SubtitleCache::new(1_024, 64);
        assert!(cache
            .get(Path::new("/x"), 0, 0, SubtitleKind::Embedded)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn event_windows_round_trip_and_fail_open() {
        // B46 — a stored timeline comes back Known; anything else (miss,
        // no pool, scan pending/failed) is Unknown so the caller burns
        // conservatively — gating must never LOSE a subtitle.
        let cache = SubtitleCache::new(65_536, 64);
        let p = Path::new("/media/movie.mkv");

        // Unknown on first ask (kicks the background scan, which — with no
        // pool attached — resolves to a once-per-boot Failed marker).
        assert!(matches!(
            cache.image_sub_event_windows(p, 7, 0).await,
            EventWindows::Unknown
        ));

        // A stored timeline round-trips.
        let windows: Vec<(u64, u64)> = vec![(2_000, 4_000), (14_000, 16_000)];
        cache
            .store(
                p,
                7,
                0,
                SubtitleKind::EventWindows,
                serde_json::to_vec(&windows).unwrap(),
            )
            .await;
        match cache.image_sub_event_windows(p, 7, 0).await {
            EventWindows::Known(w) => assert_eq!(*w, windows),
            other => panic!("expected Known, got {other:?}"),
        }

        // A different mtime is a different key (edited file → rescan).
        assert!(matches!(
            cache.image_sub_event_windows(p, 8, 0).await,
            EventWindows::Unknown
        ));
        // Corrupt persisted bytes → Unknown (rescan path), not a panic.
        cache
            .store(p, 9, 0, SubtitleKind::EventWindows, b"not json".to_vec())
            .await;
        assert!(matches!(
            cache.image_sub_event_windows(p, 9, 0).await,
            EventWindows::Unknown
        ));
    }

    #[tokio::test]
    async fn disk_layer_survives_a_restart() {
        // The whole point of persistence: a subtitle extracted once must not be
        // re-demuxed (~30 s over NFS) after a pod restart. Model the restart as
        // a FRESH cache (empty memory) pointed at the same disk root.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("cache");
        let bytes = b"WEBVTT\n\n00:00.000 --> 00:02.000\nhi\n".to_vec();

        let first = SubtitleCache::new(1_024 * 1_024, 64).with_disk(&root);
        first
            .store(
                Path::new("/m/x.mkv"),
                7,
                3,
                SubtitleKind::EmbeddedAss,
                bytes.clone(),
            )
            .await;

        // Fresh instance, cold memory, same disk → must hit disk (no re-extract).
        let restarted = SubtitleCache::new(1_024 * 1_024, 64).with_disk(&root);
        assert_eq!(restarted.entry_count().await, 0, "memory starts cold");
        let got = restarted
            .get(Path::new("/m/x.mkv"), 7, 3, SubtitleKind::EmbeddedAss)
            .await;
        assert_eq!(
            got.as_deref(),
            Some(&bytes),
            "disk layer must serve after restart"
        );
        // And it promoted into the hot memory map.
        assert_eq!(restarted.entry_count().await, 1);

        // A different mtime (edited file) must NOT match the stale disk entry.
        assert!(restarted
            .get(Path::new("/m/x.mkv"), 8, 3, SubtitleKind::EmbeddedAss)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn memory_only_when_no_disk_root() {
        // Without with_disk, nothing persists — a fresh instance is empty.
        let dir = tempfile::TempDir::new().unwrap();
        let _ = dir; // no disk root used
        let cache = SubtitleCache::new(1_024, 64);
        cache
            .store(Path::new("/x"), 1, 0, SubtitleKind::Embedded, b"x".to_vec())
            .await;
        let fresh = SubtitleCache::new(1_024, 64);
        assert!(fresh
            .get(Path::new("/x"), 1, 0, SubtitleKind::Embedded)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn store_then_get_returns_same_bytes() {
        let cache = SubtitleCache::new(1_024, 64);
        let bytes = b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello\n".to_vec();
        cache
            .store(
                Path::new("/x"),
                42,
                0,
                SubtitleKind::Embedded,
                bytes.clone(),
            )
            .await;
        let got = cache
            .get(Path::new("/x"), 42, 0, SubtitleKind::Embedded)
            .await
            .unwrap();
        assert_eq!(got.as_slice(), bytes.as_slice());
    }

    #[tokio::test]
    async fn srt_kind_is_a_distinct_key_from_webvtt() {
        // T97/B72: the SRT delivery form caches under its own key so it never
        // collides with the WebVTT extraction of the SAME track (same index,
        // different bytes).
        let cache = SubtitleCache::new(1_024, 64);
        cache
            .store(
                Path::new("/x"),
                1,
                3,
                SubtitleKind::Embedded,
                b"WEBVTT".to_vec(),
            )
            .await;
        cache
            .store(
                Path::new("/x"),
                1,
                3,
                SubtitleKind::EmbeddedSrt,
                b"1\n00:00:00,000 --> 00:00:01,000\nhi\n".to_vec(),
            )
            .await;
        let vtt = cache
            .get(Path::new("/x"), 1, 3, SubtitleKind::Embedded)
            .await
            .unwrap();
        let srt = cache
            .get(Path::new("/x"), 1, 3, SubtitleKind::EmbeddedSrt)
            .await
            .unwrap();
        assert_eq!(vtt.as_slice(), b"WEBVTT");
        assert_ne!(vtt.as_slice(), srt.as_slice(), "SRT must not alias WebVTT");
    }

    #[tokio::test]
    async fn mtime_change_invalidates_entry() {
        let cache = SubtitleCache::new(1_024, 64);
        cache
            .store(
                Path::new("/x"),
                42,
                0,
                SubtitleKind::Embedded,
                b"old".to_vec(),
            )
            .await;
        // Different mtime → different key → miss.
        assert!(cache
            .get(Path::new("/x"), 43, 0, SubtitleKind::Embedded)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn lru_evicts_least_recent_when_over_cap() {
        let cache = SubtitleCache::new(10, 64);
        cache
            .store(Path::new("/a"), 1, 0, SubtitleKind::Embedded, vec![0u8; 6])
            .await;
        cache
            .store(Path::new("/b"), 1, 0, SubtitleKind::Embedded, vec![0u8; 6])
            .await;
        // 12 bytes total > 10 cap → /a (oldest) evicted.
        assert!(cache.total_bytes().await <= 10);
        assert!(cache
            .get(Path::new("/a"), 1, 0, SubtitleKind::Embedded)
            .await
            .is_none());
        assert!(cache
            .get(Path::new("/b"), 1, 0, SubtitleKind::Embedded)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn entry_cap_caps_count_independent_of_bytes() {
        let cache = SubtitleCache::new(u64::MAX, 2);
        for n in 0..5u32 {
            cache
                .store(Path::new("/x"), 1, n, SubtitleKind::Embedded, vec![0u8; 1])
                .await;
        }
        assert_eq!(cache.entry_count().await, 2);
    }
}
