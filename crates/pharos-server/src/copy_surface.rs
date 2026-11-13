//! The server half of the H.264 copy surface (spec 041, ADR-0123).
//!
//! The worker indexes a source once: its keyframe plan, its `avcC`, and a
//! check that the first segments copy cleanly. The verdict is kept here in
//! memory, keyed by the file's path, size and mtime, so a replaced file is
//! indexed again. Segments are read by a pool of their own, so a playback
//! never queues behind a trickplay or subtitle sweep on the shared pool.
//!
//! # Signals
//!
//! ```text
//! pharos_copy_surface_decision_total{verdict,reason}  verdict: copy | encode
//! pharos_copy_segment_total{outcome,reason}           outcome: served | failed
//! pharos_copy_segment_seconds                         worker read + write, per segment
//! ```

use pharos_transcode::copy_surface::{CopyIndexVerdict, CopyRefusal, CopySegmentReport};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

/// Workers in the copy pool. A copy segment is a read of a few MB, so a few
/// workers serve every concurrent playback; the cap only bounds a burst.
pub const COPY_POOL_WORKERS: usize = 4;

/// How long the master playlist waits for a first index before it serves the
/// encode surface. The index reads the cues and three segments, well under a
/// second on local disk; past this the playback should not wait on it.
pub const MASTER_INDEX_WAIT: Duration = Duration::from_secs(2);

/// Verdicts kept before the map is cleared. One per file played since the
/// server started; a verdict is a few KB, so this bounds the map near 4 MB.
const MAX_VERDICTS: usize = 1024;

/// What identifies one version of a source file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileKey {
    path: PathBuf,
    len: u64,
    mtime_ns: i128,
}

impl FileKey {
    async fn of(path: &Path) -> Result<Self, CopyError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| CopyError::Stat(format!("{}: {e}", path.display())))?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        Ok(Self {
            path: path.to_owned(),
            len: meta.len(),
            mtime_ns,
        })
    }
}

/// One indexed source: the worker's verdict and the token that names its plan
/// in every playlist and segment URL.
#[derive(Debug)]
pub struct Indexed {
    pub verdict: CopyIndexVerdict,
    /// Changes whenever the file or its plan does, so a segment URL from an
    /// older plan never reads a range of a newer one.
    pub token: String,
}

/// Why the copy surface could not answer. Never cached: none of these says
/// anything about the file itself.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CopyError {
    #[error("stat: {0}")]
    Stat(String),
    /// The worker crashed, timed out or could not be spawned.
    #[error("worker: {0}")]
    Worker(String),
    /// The worker read the file and refused it: a fact about the source.
    #[error("source: {0}")]
    Source(String),
    #[error("read segment file: {0}")]
    Read(String),
    #[error("copy surface is not available on this platform")]
    Unsupported,
}

impl CopyError {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stat(_) => "stat",
            Self::Worker(_) => "worker",
            Self::Source(_) => "source",
            Self::Read(_) => "read",
            Self::Unsupported => "unsupported",
        }
    }
}

type Cell = Arc<OnceCell<Arc<Indexed>>>;

/// A segment's scratch file, removed when dropped.
struct ScratchFile(PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(file = %self.0.display(), error = %e, "copy surface: segment file not removed");
            }
        }
    }
}

/// The copy surface's index cache and its worker pool.
pub struct CopySurface {
    #[cfg(unix)]
    pool: pharos_transcode::worker::LibavWorkerPool,
    /// Private to this process (mode 0700), so no other user can plant a
    /// link at a segment's predictable name. Removed on drop.
    scratch: tempfile::TempDir,
    next_file: AtomicU64,
    verdicts: Mutex<HashMap<FileKey, Cell>>,
}

impl std::fmt::Debug for CopySurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopySurface")
            .field("scratch", &self.scratch.path())
            .finish_non_exhaustive()
    }
}

impl CopySurface {
    /// A directory of its own under `scratch_parent` holds each segment for
    /// the moment between the worker writing it and the handler reading it
    /// back.
    #[cfg(unix)]
    pub fn new(
        pool: pharos_transcode::worker::LibavWorkerPool,
        scratch_parent: &Path,
    ) -> std::io::Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("pharos-copy-")
            .tempdir_in(scratch_parent)?;
        Ok(Self {
            pool,
            scratch,
            next_file: AtomicU64::new(0),
            verdicts: Mutex::new(HashMap::new()),
        })
    }

    fn cell(&self, key: FileKey) -> Cell {
        let mut map = self
            .verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !map.contains_key(&key) && map.len() >= MAX_VERDICTS {
            map.clear();
        }
        map.entry(key).or_default().clone()
    }

    /// The source's verdict, indexing it on the first ask. Concurrent asks for
    /// one file share one index; a failed index is not remembered.
    pub async fn index(&self, path: &Path) -> Result<Arc<Indexed>, CopyError> {
        let key = FileKey::of(path).await?;
        let cell = self.cell(key.clone());
        cell.get_or_try_init(|| async {
            let started = Instant::now();
            let verdict = self.run_index(path).await?;
            let token = plan_token(&key, &verdict);
            match &verdict {
                CopyIndexVerdict::Copyable(s) => tracing::info!(
                    path = %path.display(),
                    segments = s.plan.segments.len(),
                    target_duration = s.plan.target_duration_secs(),
                    copy.token = %token,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "copy surface: source indexed, copyable"
                ),
                CopyIndexVerdict::Refused(r) => tracing::info!(
                    path = %path.display(),
                    reason = r.label(),
                    detail = %r,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "copy surface: source indexed, refused"
                ),
            }
            Ok(Arc::new(Indexed { verdict, token }))
        })
        .await
        .cloned()
    }

    /// [`Self::index`], waiting at most `wait`. The index keeps running after
    /// a timeout, so the next playback of the file finds it done.
    pub async fn index_within(
        self: &Arc<Self>,
        path: &Path,
        wait: Duration,
    ) -> Option<Result<Arc<Indexed>, CopyError>> {
        let this = Arc::clone(self);
        let path = path.to_owned();
        let task = tokio::spawn(async move { this.index(&path).await });
        match tokio::time::timeout(wait, task).await {
            Ok(Ok(r)) => Some(r),
            Ok(Err(join)) => Some(Err(CopyError::Worker(format!("index task: {join}")))),
            Err(_) => None,
        }
    }

    /// Mark the file refused after a segment failed mid-file, so later
    /// playbacks of it get the encode surface.
    pub async fn refuse(&self, path: &Path, refusal: CopyRefusal) {
        let Ok(key) = FileKey::of(path).await else {
            return;
        };
        let verdict = CopyIndexVerdict::Refused(refusal);
        let token = plan_token(&key, &verdict);
        let cell = Arc::new(OnceCell::new_with(Some(Arc::new(Indexed {
            verdict,
            token,
        }))));
        self.verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, cell);
    }

    /// Copy plan segment `index` and return its bytes.
    pub async fn segment(
        &self,
        path: &Path,
        index: u32,
        segment: pharos_core::copy_plan::CopySegment,
        last: bool,
    ) -> Result<(Vec<u8>, CopySegmentReport), CopyError> {
        let n = self.next_file.fetch_add(1, Ordering::Relaxed);
        // Removes the file on every exit, including a client disconnect that
        // drops this future mid-await.
        let out = ScratchFile(self.scratch.path().join(format!("copy-{n}.m4s")));
        let report = self.run_segment(path, index, segment, last, &out.0).await?;
        let bytes = tokio::fs::read(&out.0)
            .await
            .map_err(|e| CopyError::Read(format!("{}: {e}", out.0.display())))?;
        Ok((bytes, report))
    }

    #[cfg(unix)]
    async fn run_index(&self, path: &Path) -> Result<CopyIndexVerdict, CopyError> {
        self.pool
            .copy_index(path)
            .await
            .map_err(|e| CopyError::Worker(e.to_string()))
    }

    #[cfg(not(unix))]
    async fn run_index(&self, _path: &Path) -> Result<CopyIndexVerdict, CopyError> {
        Err(CopyError::Unsupported)
    }

    #[cfg(unix)]
    async fn run_segment(
        &self,
        path: &Path,
        index: u32,
        segment: pharos_core::copy_plan::CopySegment,
        last: bool,
        out: &Path,
    ) -> Result<CopySegmentReport, CopyError> {
        self.pool
            .copy_segment(path, index, segment, last, out)
            .await
            .map_err(|e| match e {
                pharos_transcode::worker::PoolError::Op(op) => CopyError::Source(op.to_string()),
                other => CopyError::Worker(other.to_string()),
            })
    }

    #[cfg(not(unix))]
    async fn run_segment(
        &self,
        _path: &Path,
        _index: u32,
        _segment: pharos_core::copy_plan::CopySegment,
        _last: bool,
        _out: &Path,
    ) -> Result<CopySegmentReport, CopyError> {
        Err(CopyError::Unsupported)
    }
}

/// Bumped whenever the bytes written for a given plan change, so a client
/// holding an `immutable` segment from an older writer fetches it again.
const COPY_WRITER_VERSION: u32 = 1;

/// 16 hex digits over the writer version, the file's identity and the
/// verdict, plan included.
fn plan_token(key: &FileKey, verdict: &CopyIndexVerdict) -> String {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&COPY_WRITER_VERSION.to_le_bytes());
    h.update(key.path.as_os_str().as_encoded_bytes());
    h.update(&key.len.to_le_bytes());
    h.update(&key.mtime_ns.to_le_bytes());
    if let CopyIndexVerdict::Copyable(s) = verdict {
        h.update(&s.plan.timescale.to_le_bytes());
        h.update(&s.origin.to_le_bytes());
        for seg in &s.plan.segments {
            h.update(&seg.start.to_le_bytes());
            h.update(&seg.end.to_le_bytes());
        }
    }
    format!("{:016x}", h.digest())
}

/// Why a play session that asked for the copy surface got the encode surface,
/// before any index was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyBlocker {
    /// The master is not the browser demuxed-h264 one.
    NotCmaf,
    /// A split film plays its parts through one playlist.
    SplitParts,
    Codec,
    BitrateCap,
    OutputSize,
    Stereo,
    Burn,
    Tonemap,
}

impl CopyBlocker {
    pub const ALL: [Self; 8] = [
        Self::NotCmaf,
        Self::SplitParts,
        Self::Codec,
        Self::BitrateCap,
        Self::OutputSize,
        Self::Stereo,
        Self::Burn,
        Self::Tonemap,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NotCmaf => "not_cmaf",
            Self::SplitParts => "split_parts",
            Self::Codec => "codec",
            Self::BitrateCap => "bitrate_cap",
            Self::OutputSize => "output_size",
            Self::Stereo => "stereo",
            Self::Burn => "burn",
            Self::Tonemap => "tonemap",
        }
    }
}

/// The master's verdict for a session that asked for the copy surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyDecision {
    Copy,
    Blocked(CopyBlocker),
    Refused(&'static str),
    IndexTimeout,
    IndexError(&'static str),
}

impl CopyDecision {
    pub const fn verdict(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            _ => "encode",
        }
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Self::Copy => "eligible",
            Self::Blocked(b) => b.label(),
            Self::Refused(label) => label,
            Self::IndexTimeout => "index_timeout",
            Self::IndexError(label) => label,
        }
    }

    pub fn record(self) {
        metrics::counter!(
            "pharos_copy_surface_decision_total",
            "verdict" => self.verdict(),
            "reason" => self.reason(),
        )
        .increment(1);
    }
}

/// Count one copy segment request.
pub fn record_segment(outcome: &'static str, reason: &'static str, elapsed: Duration) {
    metrics::counter!(
        "pharos_copy_segment_total",
        "outcome" => outcome,
        "reason" => reason,
    )
    .increment(1);
    if outcome == "served" {
        metrics::histogram!("pharos_copy_segment_seconds").record(elapsed.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_core::copy_plan::{CopyPlan, CopySegment};
    use pharos_transcode::copy_fmp4::CopyTrack;
    use pharos_transcode::copy_surface::CopySource;

    fn key() -> FileKey {
        FileKey {
            path: PathBuf::from("/m/a.mkv"),
            len: 10,
            mtime_ns: 1,
        }
    }

    fn copyable(end: i64) -> CopyIndexVerdict {
        CopyIndexVerdict::Copyable(Box::new(CopySource {
            track: CopyTrack {
                avcc: vec![1, 100, 0, 13, 0xff, 0xe0, 0],
                width: 2,
                height: 2,
                sar: None,
                timescale: 1000,
            },
            plan: CopyPlan {
                timescale: 1000,
                segments: vec![CopySegment { start: 0, end }],
            },
            origin: 0,
        }))
    }

    #[test]
    fn the_token_follows_the_file_and_the_plan() {
        let base = plan_token(&key(), &copyable(6000));
        assert_eq!(base.len(), 16);
        assert_eq!(base, plan_token(&key(), &copyable(6000)));
        assert_ne!(base, plan_token(&key(), &copyable(7000)));
        let mut touched = key();
        touched.mtime_ns = 2;
        assert_ne!(base, plan_token(&touched, &copyable(6000)));
    }

    #[test]
    fn blocker_labels_are_distinct() {
        let mut labels: Vec<_> = CopyBlocker::ALL.iter().map(|b| b.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), CopyBlocker::ALL.len());
    }

    #[test]
    fn only_copy_is_a_copy_verdict() {
        assert_eq!(CopyDecision::Copy.verdict(), "copy");
        assert_eq!(CopyDecision::IndexTimeout.verdict(), "encode");
        assert_eq!(CopyDecision::Blocked(CopyBlocker::Burn).reason(), "burn");
    }
}
