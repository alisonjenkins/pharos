//! Persistent libav-worker pool (P48 Phase 2).
//!
//! The high-frequency "tiny ops" (probe, image, trickplay, subtitle,
//! waveform) run inside long-lived `transcode-worker` subprocesses built
//! with `backend-lib`. Unlike the segment scheduler — which spawns a
//! worker per encode — this pool keeps workers **resident**, so the
//! fork/exec is paid once per worker and amortised across many ops. A
//! libav fault (segfault) kills only the worker process; the pool sees
//! EOF, discards it, and spawns a fresh one on the next request. The
//! server process is never affected (V6).
//!
//! Each request is single-flight on its worker: send one `WorkerCmd::Tiny`
//! frame, read control frames until the matching terminal reply. A
//! bounded `Semaphore` caps concurrent in-flight ops (and thus live
//! workers). On any channel error / EOF / timeout the worker is dropped
//! (its `Child` has `kill_on_drop`) rather than returned to the idle set.

use crate::protocol::{
    read_frame, write_frame, JobId, TinyOp, WorkerCmd, WorkerError, WorkerEvent,
};
use crate::worker::proc::spawn_worker_proc;
use pharos_core::ProbeInfo;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::Child;
use tokio::sync::{Mutex, Semaphore};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Tiny ops are bounded work (decode a header / one frame / a few seconds
/// of audio). A worker that misses this is assumed hung → dropped.
const DEFAULT_OP_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MAX_WORKERS: usize = 4;

/// Errors surfaced to callers of the pool.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// The op ran but the worker reported a failure (malformed input,
    /// unsupported codec, internal error). Carries the wire error.
    #[error("worker op failed: {0}")]
    Op(WorkerError),
    /// Could not spawn / hand-shake a worker.
    #[error("spawn worker: {0}")]
    Spawn(String),
    /// The worker died, hung, or spoke out of protocol mid-op. The op may
    /// be safely retried (a fresh worker will be spawned).
    #[error("worker died mid-op: {0}")]
    Dead(String),
}

/// One resident worker checked out of / into the idle set.
#[derive(Debug)]
struct PooledWorker {
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    // Kept for `kill_on_drop`: dropping the worker reaps the child.
    _child: Child,
}

#[derive(Debug)]
struct Inner {
    worker_bin: PathBuf,
    handshake_timeout: Duration,
    op_timeout: Duration,
    idle: Mutex<Vec<PooledWorker>>,
    permits: Semaphore,
    next_job: AtomicU64,
}

/// A cheap-to-clone handle to the resident libav-worker pool.
#[derive(Clone, Debug)]
pub struct LibavWorkerPool {
    inner: Arc<Inner>,
}

impl LibavWorkerPool {
    /// Build a pool that launches the given `transcode-worker` binary
    /// (must be a `backend-lib` build). `max_workers` caps concurrent
    /// in-flight ops + resident workers.
    pub fn new(worker_bin: impl Into<PathBuf>, max_workers: usize) -> Self {
        let max = max_workers.max(1);
        Self {
            inner: Arc::new(Inner {
                worker_bin: worker_bin.into(),
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                op_timeout: DEFAULT_OP_TIMEOUT,
                idle: Mutex::new(Vec::new()),
                permits: Semaphore::new(max),
                next_job: AtomicU64::new(1),
            }),
        }
    }

    /// Pool sized to the machine, discovering the worker binary the same
    /// way `ProcSpawner` does (env → sibling → PATH).
    pub fn with_discovered_bin() -> Self {
        let bin = crate::worker::ProcSpawner::new().worker_bin().to_path_buf();
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(DEFAULT_MAX_WORKERS);
        Self::new(bin, max)
    }

    /// Override the per-op timeout. Only effective before the pool is
    /// cloned (no-op afterwards, since `Inner` is then shared).
    pub fn with_op_timeout(mut self, d: Duration) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.op_timeout = d;
        }
        self
    }

    /// Probe a media file in-process. The standout scan hotspot.
    pub async fn probe(&self, input: impl Into<PathBuf>) -> Result<ProbeInfo, PoolError> {
        let ev = self
            .run(TinyOp::Probe {
                input: input.into(),
            })
            .await?;
        match ev {
            WorkerEvent::ProbeResult { info, .. } => Ok(*info),
            other => Err(unexpected(other)),
        }
    }

    /// Extract a single scaled JPEG frame to `out`.
    pub async fn extract_image(
        &self,
        input: impl Into<PathBuf>,
        seek_ms: Option<u64>,
        width: u32,
        quality: i32,
        out: impl Into<PathBuf>,
    ) -> Result<(), PoolError> {
        let ev = self
            .run(TinyOp::Image {
                input: input.into(),
                seek_ms,
                width,
                quality,
                out: out.into(),
            })
            .await?;
        expect_done(ev).map(|_| ())
    }

    /// Generate trickplay sprite sheets; returns the sheet count produced.
    #[allow(clippy::too_many_arguments)]
    pub async fn trickplay(
        &self,
        input: impl Into<PathBuf>,
        interval_ms: u64,
        width: u32,
        grid: u32,
        max_sheets: u32,
        quality: i32,
        out_dir: impl Into<PathBuf>,
    ) -> Result<u32, PoolError> {
        let ev = self
            .run(TinyOp::Trickplay {
                input: input.into(),
                interval_ms,
                width,
                grid,
                max_sheets,
                quality,
                out_dir: out_dir.into(),
            })
            .await?;
        expect_done(ev).map(|n| n as u32)
    }

    /// Convert a SubRip sidecar to WebVTT, written to `out`.
    pub async fn srt_to_webvtt(
        &self,
        input: impl Into<PathBuf>,
        out: impl Into<PathBuf>,
    ) -> Result<(), PoolError> {
        let ev = self
            .run(TinyOp::SrtToWebvtt {
                input: input.into(),
                out: out.into(),
            })
            .await?;
        expect_done(ev).map(|_| ())
    }

    /// Audio RMS waveform — per-bin dBFS.
    pub async fn waveform(
        &self,
        input: impl Into<PathBuf>,
        samples_per_bin: u64,
        target_bins: u32,
    ) -> Result<Vec<f32>, PoolError> {
        let ev = self
            .run(TinyOp::Waveform {
                input: input.into(),
                samples_per_bin,
                target_bins,
            })
            .await?;
        match ev {
            WorkerEvent::WaveformResult { bins, .. } => Ok(bins),
            other => Err(unexpected(other)),
        }
    }

    /// Core request/reply: acquire a permit, check out (or spawn) a
    /// worker, run one op, and return the terminal `WorkerEvent`. A
    /// healthy worker is returned to the idle set; a broken one is dropped.
    async fn run(&self, op: TinyOp) -> Result<WorkerEvent, PoolError> {
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|e| PoolError::Dead(format!("pool closed: {e}")))?;

        let job_id = JobId(self.inner.next_job.fetch_add(1, Ordering::Relaxed));
        let mut worker = self.checkout().await?;

        match self.exchange(&mut worker, job_id, op).await {
            Ok(ev) => {
                // Healthy → reuse.
                self.inner.idle.lock().await.push(worker);
                match ev {
                    WorkerEvent::Failed { error, .. } => Err(PoolError::Op(error)),
                    other => Ok(other),
                }
            }
            Err(e) => {
                // Broken channel → drop the worker (kill_on_drop reaps it).
                drop(worker);
                Err(e)
            }
        }
    }

    /// Take an idle worker, else spawn a fresh one.
    async fn checkout(&self) -> Result<PooledWorker, PoolError> {
        if let Some(w) = self.inner.idle.lock().await.pop() {
            return Ok(w);
        }
        let (child, rd, wr, _handshake) =
            spawn_worker_proc(&self.inner.worker_bin, self.inner.handshake_timeout, false)
                .await
                .map_err(|e| PoolError::Spawn(e.to_string()))?;
        Ok(PooledWorker {
            rd,
            wr,
            _child: child,
        })
    }

    /// Send the op and read frames until the terminal reply for `job_id`,
    /// bounded by `op_timeout`. Non-terminal frames (a stray Progress /
    /// Accepted) are skipped.
    async fn exchange(
        &self,
        worker: &mut PooledWorker,
        job_id: JobId,
        op: TinyOp,
    ) -> Result<WorkerEvent, PoolError> {
        write_frame(&mut worker.wr, &WorkerCmd::Tiny { job_id, op })
            .await
            .map_err(|e| PoolError::Dead(format!("send op: {e}")))?;

        let deadline = tokio::time::Instant::now() + self.inner.op_timeout;
        loop {
            let frame =
                tokio::time::timeout_at(deadline, read_frame::<_, WorkerEvent>(&mut worker.rd))
                    .await
                    .map_err(|_| PoolError::Dead("op timeout".into()))?
                    .map_err(|e| PoolError::Dead(format!("read reply: {e}")))?;
            let ev = match frame {
                Some(ev) => ev,
                None => return Err(PoolError::Dead("worker closed mid-op".into())),
            };
            // Match the terminal replies; ignore heartbeat-ish frames.
            match &ev {
                WorkerEvent::ProbeResult { job_id: j, .. }
                | WorkerEvent::WaveformResult { job_id: j, .. }
                | WorkerEvent::Done { job_id: j, .. }
                | WorkerEvent::Failed { job_id: j, .. } => {
                    if *j != job_id {
                        return Err(PoolError::Dead(format!(
                            "reply for {j} but expected {job_id}"
                        )));
                    }
                    return Ok(ev);
                }
                // Accepted/Progress/Hello — not terminal for a tiny op.
                _ => continue,
            }
        }
    }
}

fn expect_done(ev: WorkerEvent) -> Result<u64, PoolError> {
    match ev {
        WorkerEvent::Done { out_bytes, .. } => Ok(out_bytes),
        other => Err(unexpected(other)),
    }
}

fn unexpected(ev: WorkerEvent) -> PoolError {
    PoolError::Dead(format!("unexpected reply: {ev:?}"))
}
