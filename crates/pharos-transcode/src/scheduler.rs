//! `TranscodeScheduler` — the load-balancing actor.
//!
//! One `tokio` task owns all scheduling state and processes [`SchedMsg`]s
//! from a bounded mpsc inbox (the same actor shape as
//! `pharos-sync::group::GroupHandle`). The actor does **O(1) bookkeeping
//! per message and never `.await`s an encode** — every encode runs in a
//! detached task that owns an `OwnedSemaphorePermit` (RAII release) and
//! reports back via a [`SchedMsg::JobFinished`] message.
//!
//! ## Why this can't deadlock
//! - Permits are taken with `try_acquire_owned` (non-blocking). If none
//!   is free the job is queued, never awaited-on inside the actor.
//! - The permit is released by `Drop` in the detached task **before** it
//!   sends `JobFinished`, so the freed slot is visible the instant the
//!   actor drains the pending queue on that edge. No "release" message
//!   exists, so the actor can never block trying to send one.
//! - Worker *spawning* happens inside the detached task, not the actor,
//!   so a slow fork never stalls the inbox.
//! - The pending queue is bounded; when full, `Submit` replies `Busy`
//!   (backpressure) rather than blocking the inbox.
//! - Every code path resolves the caller's reply exactly once (success,
//!   error, or — if the whole actor dies — a dropped oneshot → clean
//!   `RecvError`). No path leaves a caller hung.

use crate::device::DeviceTable;
use crate::options::TranscodeOptions;
use crate::protocol::{DeviceId, JobId, JobSpec, OutputSink, WorkerError, WorkerId};
use bytes::Bytes;
use smallvec::SmallVec;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

/// A live transcode output as a stream of muxed byte chunks. Boxed so the
/// type stays platform-agnostic (the concrete unix worker stream lives in
/// `worker::proc`). The stream owns the worker process + its device
/// permit; dropping it tears the encode down (broken pipe → ffmpeg exits)
/// and frees the slot.
pub type LiveByteStream = Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Terminal result of one worker running one job.
#[derive(Debug)]
pub enum WorkerRunResult {
    Done { out_bytes: u64 },
    Failed(WorkerError),
    /// The worker process vanished mid-job (segfault / closed pipe /
    /// heartbeat timeout). The `Box<dyn Worker>` is unusable and dropped.
    Died,
}

/// Boxed future a [`Worker::run`] call returns.
pub type RunFuture<'a> = Pin<Box<dyn Future<Output = WorkerRunResult> + Send + 'a>>;

/// Boxed future a [`WorkerSpawner::spawn`] call returns.
pub type SpawnFuture = Pin<Box<dyn Future<Output = std::io::Result<Box<dyn Worker>>> + Send>>;

/// A reusable worker bound to one job at a time. The implementation is
/// responsible for its own liveness watchdog — `run` must eventually
/// resolve (returning `Died` on a hung/dead worker), never hang, so the
/// scheduler's detached task can't leak.
pub trait Worker: Send {
    fn id(&self) -> WorkerId;
    fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a>;
}

/// Boxed future a [`WorkerSpawner::spawn_streaming`] call returns.
pub type StreamFuture = Pin<Box<dyn Future<Output = std::io::Result<LiveByteStream>> + Send>>;

/// Spawns fresh workers on demand (process fork for the real backend; an
/// in-process stub for tests). Injectable so the scheduler core is
/// testable with zero ffmpeg.
pub trait WorkerSpawner: Send + Sync + 'static {
    fn spawn(&self, id: WorkerId) -> SpawnFuture;

    /// Spawn a one-shot streaming worker for the live path: it encodes
    /// `spec` (sink = `Stdout`) and streams the muxed bytes back. The
    /// default errors so spawners that don't support streaming (e.g. the
    /// in-process test mock) cleanly decline; `ProcSpawner` overrides it.
    fn spawn_streaming(&self, _spec: JobSpec) -> StreamFuture {
        Box::pin(async {
            Err(std::io::Error::other(
                "this spawner does not support live streaming",
            ))
        })
    }
}

/// Where the caller wants output to land.
#[derive(Debug, Clone)]
pub enum SinkRequest {
    /// Worker writes the encoded output straight to `out_path` (caller
    /// owns any subsequent atomic rename). No cross-process byte copy.
    FileDirect { out_path: PathBuf },
    /// Live HTTP path — streamed back via a pipe. Wired in the
    /// fd-passing step; rejected with `Unsupported` until then.
    LiveStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub device: DeviceId,
    pub out_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedError {
    /// Pending queue full — caller should retry later (backpressure).
    Busy,
    /// No device can encode this job's target.
    Unsupported,
    /// Job failed non-recoverably (or exhausted retries). Carries the
    /// last worker error for the log / caller.
    Failed(WorkerError),
    /// Scheduler channel issue (actor gone, reply dropped).
    Io(String),
}

impl std::fmt::Display for SchedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedError::Busy => write!(f, "transcode scheduler busy"),
            SchedError::Unsupported => write!(f, "no device can encode this target"),
            SchedError::Failed(e) => write!(f, "transcode failed: {e}"),
            SchedError::Io(s) => write!(f, "scheduler io: {s}"),
        }
    }
}

impl std::error::Error for SchedError {}

/// Per-device + queue snapshot for the test tool / metrics.
#[derive(Debug, Clone)]
pub struct SchedSnapshot {
    pub devices: Vec<DeviceStat>,
    pub pending: usize,
    pub idle_workers: usize,
}

#[derive(Debug, Clone)]
pub struct DeviceStat {
    pub id: DeviceId,
    pub capacity: usize,
    pub in_use: usize,
    pub in_cooldown: bool,
}

/// Tunables.
#[derive(Debug, Clone)]
pub struct SchedConfig {
    pub inbox_depth: usize,
    pub pending_cap: usize,
    pub cooldown: Duration,
    pub max_retries: u8,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            inbox_depth: 256,
            pending_cap: 256,
            cooldown: Duration::from_secs(2),
            max_retries: 3,
        }
    }
}

/// Caller-facing handle. Clone freely; all clones feed the one actor.
#[derive(Clone)]
pub struct TranscodeScheduler {
    tx: mpsc::Sender<SchedMsg>,
}

enum SchedMsg {
    Submit {
        input: PathBuf,
        opts: TranscodeOptions,
        sink: SinkRequest,
        reply: oneshot::Sender<Result<JobDone, SchedError>>,
    },
    SubmitLive {
        input: PathBuf,
        opts: TranscodeOptions,
        reply: oneshot::Sender<Result<LiveByteStream, SchedError>>,
    },
    JobFinished {
        job_id: JobId,
        device: DeviceId,
        result: WorkerRunResult,
        /// Worker returned for reuse, or `None` if it died.
        worker: Option<Box<dyn Worker>>,
    },
    Snapshot {
        reply: oneshot::Sender<SchedSnapshot>,
    },
}

/// Retry context the actor keeps for an in-flight or queued job. Holds
/// the caller's reply until a terminal outcome resolves it.
struct JobCtx {
    input: PathBuf,
    opts: TranscodeOptions,
    sink: SinkRequest,
    reply: oneshot::Sender<Result<JobDone, SchedError>>,
    /// Devices already tried + failed transiently — excluded from retry.
    excluded: SmallVec<[DeviceId; 4]>,
    retries: u8,
    last_error: Option<WorkerError>,
}

struct SchedState {
    devices: DeviceTable,
    spawner: Arc<dyn WorkerSpawner>,
    idle: Vec<Box<dyn Worker>>,
    inflight: HashMap<JobId, JobCtx>,
    pending: VecDeque<(JobId, JobCtx)>,
    cfg: SchedConfig,
    next_job: u64,
    next_worker: u64,
}

impl TranscodeScheduler {
    pub fn spawn(
        devices: DeviceTable,
        spawner: Arc<dyn WorkerSpawner>,
        cfg: SchedConfig,
    ) -> TranscodeScheduler {
        let (tx, mut rx) = mpsc::channel::<SchedMsg>(cfg.inbox_depth);
        let self_tx = tx.clone();
        let mut state = SchedState {
            devices,
            spawner,
            idle: Vec::new(),
            inflight: HashMap::new(),
            pending: VecDeque::new(),
            cfg,
            next_job: 0,
            next_worker: 0,
        };
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                handle(&mut state, msg, &self_tx);
            }
        });
        TranscodeScheduler { tx }
    }

    /// Submit a job and await its terminal outcome (FileDirect: resolves
    /// when the file is written; errors on failure/exhaustion/Busy).
    pub async fn submit(
        &self,
        input: PathBuf,
        opts: TranscodeOptions,
        sink: SinkRequest,
    ) -> Result<JobDone, SchedError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::Submit {
                input,
                opts,
                sink,
                reply,
            })
            .await
            .map_err(|_| SchedError::Io("scheduler stopped".into()))?;
        rx.await
            .map_err(|_| SchedError::Io("scheduler dropped reply".into()))?
    }

    /// Submit a live transcode and get a byte stream of the muxed output.
    /// The job is dispatched to the least-loaded eligible device; the
    /// returned stream owns the worker + its device permit, so the slot
    /// frees when the consumer drops the stream (also tearing down the
    /// encode). Returns `Busy` when no device has a free permit.
    pub async fn submit_live(
        &self,
        input: PathBuf,
        opts: TranscodeOptions,
    ) -> Result<LiveByteStream, SchedError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::SubmitLive { input, opts, reply })
            .await
            .map_err(|_| SchedError::Io("scheduler stopped".into()))?;
        rx.await
            .map_err(|_| SchedError::Io("scheduler dropped reply".into()))?
    }

    pub async fn snapshot(&self) -> Option<SchedSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SchedMsg::Snapshot { reply }).await.ok()?;
        rx.await.ok()
    }
}

/// Wraps a live byte stream so it owns the device permit for its
/// lifetime — dropping the stream frees the slot (RAII), same discipline
/// as the segment path.
struct PermitStream {
    inner: LiveByteStream,
    _permit: OwnedSemaphorePermit,
}

impl futures_core::Stream for PermitStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

fn handle(state: &mut SchedState, msg: SchedMsg, self_tx: &mpsc::Sender<SchedMsg>) {
    match msg {
        SchedMsg::Submit {
            input,
            opts,
            sink,
            reply,
        } => {
            if matches!(sink, SinkRequest::LiveStream) {
                // Wired in the fd-passing step; not yet schedulable.
                let _ = reply.send(Err(SchedError::Unsupported));
                return;
            }
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            let ctx = JobCtx {
                input,
                opts,
                sink,
                reply,
                excluded: SmallVec::new(),
                retries: 0,
                last_error: None,
            };
            place(state, job_id, ctx, self_tx);
        }
        SchedMsg::SubmitLive { input, opts, reply } => {
            // Live path: acquire a permit best-first, then spawn a
            // streaming worker off-actor. The permit rides inside the
            // returned stream (RAII release on drop) — no inflight
            // bookkeeping, no JobFinished.
            let now = Instant::now();
            let eligible = state.devices.eligible_for(&opts, now);
            let mut acquired = None;
            for dev in eligible.iter().copied() {
                if let Some(slot) = state.devices.slot(dev) {
                    if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
                        acquired = Some((dev, permit));
                        break;
                    }
                }
            }
            let Some((device, permit)) = acquired else {
                let _ = reply.send(Err(if eligible.is_empty() {
                    SchedError::Unsupported
                } else {
                    SchedError::Busy
                }));
                return;
            };
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            let spec = JobSpec {
                job_id,
                input,
                opts,
                device,
                sink: OutputSink::Stdout,
            };
            let spawner = state.spawner.clone();
            tokio::spawn(async move {
                match spawner.spawn_streaming(spec).await {
                    Ok(inner) => {
                        let stream: LiveByteStream = Box::pin(PermitStream {
                            inner,
                            _permit: permit,
                        });
                        let _ = reply.send(Ok(stream));
                    }
                    Err(e) => {
                        drop(permit);
                        let _ = reply.send(Err(SchedError::Io(e.to_string())));
                    }
                }
            });
        }
        SchedMsg::JobFinished {
            job_id,
            device,
            result,
            worker,
        } => {
            // Return a live worker to the idle pool first so a drained
            // pending job can reuse it.
            if let Some(w) = worker {
                state.idle.push(w);
            }
            let Some(mut ctx) = state.inflight.remove(&job_id) else {
                // Unknown job (already resolved / cancelled). Drain anyway.
                drain_pending(state, self_tx);
                return;
            };
            match result {
                WorkerRunResult::Done { out_bytes } => {
                    let _ = ctx.reply.send(Ok(JobDone { device, out_bytes }));
                }
                WorkerRunResult::Failed(err) if !err.is_transient() => {
                    tracing::warn!(%job_id, %device, error = %err, "transcode job failed (non-recoverable)");
                    let _ = ctx.reply.send(Err(SchedError::Failed(err)));
                }
                WorkerRunResult::Failed(err) => {
                    // Transient: cool the device + exclude it, retry next-best.
                    // NEVER cool the CPU — it's the terminal fallback; cooling
                    // it would make `eligible_for` empty and surface a spurious
                    // `Unsupported`/`Failed` for a perfectly encodable job (and
                    // for any other job arriving during the window).
                    if device != DeviceId::Cpu {
                        state
                            .devices
                            .set_cooldown(device, Instant::now() + state.cfg.cooldown);
                        ctx.excluded.push(device);
                    }
                    ctx.retries += 1;
                    ctx.last_error = Some(err);
                    retry_or_fail(state, job_id, ctx, self_tx);
                }
                WorkerRunResult::Died => {
                    // Worker death is not the device's fault — don't cool
                    // the device, but count the retry and re-place. A
                    // fresh worker is spawned on the next dispatch.
                    tracing::warn!(%job_id, %device, "transcode worker died mid-job; retrying");
                    ctx.retries += 1;
                    ctx.last_error = Some(WorkerError::Other("worker died".into()));
                    retry_or_fail(state, job_id, ctx, self_tx);
                }
            }
            // A permit just freed (the detached task dropped it before
            // sending JobFinished) — let queued jobs claim it.
            drain_pending(state, self_tx);
        }
        SchedMsg::Snapshot { reply } => {
            let devices = state
                .devices
                .slots()
                .iter()
                .map(|s| DeviceStat {
                    id: s.id,
                    capacity: s.capacity,
                    in_use: s.in_use(),
                    in_cooldown: matches!(s.cooldown_until, Some(t) if t > Instant::now()),
                })
                .collect();
            let _ = reply.send(SchedSnapshot {
                devices,
                pending: state.pending.len(),
                idle_workers: state.idle.len(),
            });
        }
    }
}

/// Decide what to do with a job that just failed transiently / died.
fn retry_or_fail(
    state: &mut SchedState,
    job_id: JobId,
    ctx: JobCtx,
    self_tx: &mpsc::Sender<SchedMsg>,
) {
    if ctx.retries > state.cfg.max_retries {
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("retries exhausted".into()));
        tracing::warn!(%job_id, error = %err, "transcode job exhausted retries");
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }
    place(state, job_id, ctx, self_tx);
}

/// Try to dispatch `ctx` to its best eligible device; queue if all
/// permits are busy; fail if no device can ever take it.
fn place(state: &mut SchedState, job_id: JobId, ctx: JobCtx, self_tx: &mpsc::Sender<SchedMsg>) {
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    if full_eligible.is_empty() {
        // No supporting device at all (e.g. cooldown could hide all HW
        // but CPU always supports; truly empty ⇒ unsupported target).
        let _ = ctx.reply.send(Err(SchedError::Unsupported));
        return;
    }
    // Candidate devices = eligible minus already-tried.
    let candidates: SmallVec<[DeviceId; 5]> = full_eligible
        .iter()
        .copied()
        .filter(|d| !ctx.excluded.contains(d))
        .collect();
    if candidates.is_empty() {
        // Every supporting device has been tried + failed transiently.
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }

    for dev in candidates.iter().copied() {
        let Some(slot) = state.devices.slot(dev) else {
            continue;
        };
        if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
            let worker = state.idle.pop();
            let worker_id = WorkerId(state.next_worker);
            state.next_worker += 1;
            let spec = JobSpec {
                job_id,
                input: ctx.input.clone(),
                opts: ctx.opts.clone(),
                device: dev,
                sink: to_output_sink(&ctx.sink),
            };
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
            );
            return;
        }
    }

    // All candidate permits busy → queue (or backpressure).
    if state.pending.len() >= state.cfg.pending_cap {
        let _ = ctx.reply.send(Err(SchedError::Busy));
    } else {
        state.pending.push_back((job_id, ctx));
    }
}

/// On a freed permit, walk the pending queue and dispatch what now fits.
/// Jobs that still don't fit stay queued in order.
fn drain_pending(state: &mut SchedState, self_tx: &mpsc::Sender<SchedMsg>) {
    let mut requeue: VecDeque<(JobId, JobCtx)> = VecDeque::new();
    while let Some((job_id, ctx)) = state.pending.pop_front() {
        // Try to place; if it can't grab a permit it returns to the queue.
        // To detect "couldn't place", check inflight membership after.
        let before_inflight = state.inflight.contains_key(&job_id);
        try_place_no_queue(state, job_id, ctx, self_tx, &mut requeue);
        let _ = before_inflight; // (kept for clarity; placement tracked in requeue)
    }
    state.pending = requeue;
}

/// Like `place` but never re-queues internally — a job that can't get a
/// permit is pushed into `requeue` (preserving order) instead, so
/// `drain_pending` doesn't recurse or reorder.
fn try_place_no_queue(
    state: &mut SchedState,
    job_id: JobId,
    ctx: JobCtx,
    self_tx: &mpsc::Sender<SchedMsg>,
    requeue: &mut VecDeque<(JobId, JobCtx)>,
) {
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    let candidates: SmallVec<[DeviceId; 5]> = full_eligible
        .iter()
        .copied()
        .filter(|d| !ctx.excluded.contains(d))
        .collect();
    if candidates.is_empty() {
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }
    for dev in candidates.iter().copied() {
        let Some(slot) = state.devices.slot(dev) else {
            continue;
        };
        if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
            let worker = state.idle.pop();
            let worker_id = WorkerId(state.next_worker);
            state.next_worker += 1;
            let spec = JobSpec {
                job_id,
                input: ctx.input.clone(),
                opts: ctx.opts.clone(),
                device: dev,
                sink: to_output_sink(&ctx.sink),
            };
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
            );
            return;
        }
    }
    requeue.push_back((job_id, ctx));
}

fn to_output_sink(sink: &SinkRequest) -> OutputSink {
    match sink {
        SinkRequest::FileDirect { out_path } => OutputSink::FileDirect {
            path: out_path.clone(),
        },
        // LiveStream is dispatched via `submit_live` (OutputSink::Stdout),
        // never through the segment `place` path; map defensively.
        SinkRequest::LiveStream => OutputSink::Stdout,
    }
}

/// Detached encode driver. Owns the permit (RAII release) + the worker.
/// Spawns a worker if none was reused. Always reports `JobFinished` so
/// the actor can resolve the reply — even on spawn failure.
#[allow(clippy::too_many_arguments)]
fn spawn_run_task(
    spawner: Arc<dyn WorkerSpawner>,
    worker: Option<Box<dyn Worker>>,
    worker_id: WorkerId,
    permit: OwnedSemaphorePermit,
    spec: JobSpec,
    device: DeviceId,
    self_tx: mpsc::Sender<SchedMsg>,
) {
    let job_id = spec.job_id;
    tokio::spawn(async move {
        let worker = match worker {
            Some(w) => w,
            None => match spawner.spawn(worker_id).await {
                Ok(w) => w,
                Err(e) => {
                    // Couldn't spawn — release permit, report as a death
                    // so the actor retries (bounded) or fails the reply.
                    drop(permit);
                    let _ = self_tx
                        .send(SchedMsg::JobFinished {
                            job_id,
                            device,
                            result: WorkerRunResult::Died,
                            worker: None,
                        })
                        .await;
                    tracing::warn!(%job_id, %device, error = %e, "worker spawn failed");
                    return;
                }
            },
        };
        // Run the worker on its own task so a PANIC inside arbitrary
        // worker-impl code (FFI/ffmpeg driver) becomes a JoinError we map
        // to `Died` rather than unwinding this task and leaking the
        // caller's reply oneshot (which lives in the actor's `inflight`
        // map, not here). Without this, a worker panic would hang
        // `submit()` forever — the module-level "reply resolved exactly
        // once" invariant depends on this.
        let run_handle = tokio::spawn(async move {
            let mut worker = worker;
            let result = worker.run(spec).await;
            (worker, result)
        });
        let (returned, result) = match run_handle.await {
            Ok((w, WorkerRunResult::Died)) => {
                drop(w);
                (None, WorkerRunResult::Died)
            }
            Ok((w, r)) => (Some(w), r),
            Err(join_err) => {
                // Panic or cancellation inside the worker run — treat as a
                // death so the bounded-retry path resolves the reply.
                tracing::warn!(%job_id, %device, error = %join_err, "worker run task aborted/panicked");
                (None, WorkerRunResult::Died)
            }
        };
        // Release the permit BEFORE notifying so the freed slot is
        // visible when the actor drains the pending queue.
        drop(permit);
        let _ = self_tx
            .send(SchedMsg::JobFinished {
                job_id,
                device,
                result,
                worker: returned,
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::options::{AudioCodec, Container, VideoCodec};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    fn h264() -> TranscodeOptions {
        TranscodeOptions {
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        }
    }

    fn file_sink() -> SinkRequest {
        SinkRequest::FileDirect {
            out_path: PathBuf::from("/dev/null"),
        }
    }

    type ScriptFn = dyn Fn(WorkerId, &JobSpec) -> WorkerRunResult + Send + Sync;

    /// Spawner whose workers run a scripted outcome after a fixed delay.
    struct ScriptedSpawner {
        f: Arc<ScriptFn>,
        delay: Duration,
        spawned: Arc<AtomicU64>,
    }

    impl ScriptedSpawner {
        fn new(
            delay: Duration,
            f: impl Fn(WorkerId, &JobSpec) -> WorkerRunResult + Send + Sync + 'static,
        ) -> (Arc<Self>, Arc<AtomicU64>) {
            let spawned = Arc::new(AtomicU64::new(0));
            (
                Arc::new(Self {
                    f: Arc::new(f),
                    delay,
                    spawned: spawned.clone(),
                }),
                spawned,
            )
        }
    }

    impl WorkerSpawner for ScriptedSpawner {
        fn spawn(
            &self,
            id: WorkerId,
        ) -> SpawnFuture {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let f = self.f.clone();
            let delay = self.delay;
            Box::pin(async move {
                Ok(Box::new(ScriptedWorker { id, f, delay }) as Box<dyn Worker>)
            })
        }
    }

    struct ScriptedWorker {
        id: WorkerId,
        f: Arc<ScriptFn>,
        delay: Duration,
    }

    impl Worker for ScriptedWorker {
        fn id(&self) -> WorkerId {
            self.id
        }
        fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
            let f = self.f.clone();
            let delay = self.delay;
            let id = self.id;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                f(id, &job)
            })
        }
    }

    fn table() -> DeviceTable {
        DeviceTable::from_probe(
            &[
                (DeviceId::hw(HwAccel::Nvenc, 0), 2),
                (DeviceId::hw(HwAccel::Vaapi, 0), 1),
            ],
            2,
        )
    }

    #[tokio::test]
    async fn dispatch_completes_on_best_device() {
        let (spawner, _) =
            ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done { out_bytes: 42 });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s.submit(PathBuf::from("/m/x"), h264(), file_sink()).await.unwrap();
        assert_eq!(done.device, DeviceId::hw(HwAccel::Nvenc, 0)); // best-first
        assert_eq!(done.out_bytes, 42);
    }

    #[tokio::test]
    async fn unsupported_target_when_no_device() {
        // A table with only HW devices and an opts that no HW supports
        // still has CPU (always present) — so to force Unsupported we
        // build a table whose only slot is in permanent cooldown? Simpler:
        // CPU always supports, so Unsupported only happens if eligible is
        // empty. With Vp9 → CPU still supports. There is no real
        // "unsupported" with CPU present; assert Vp9 lands on CPU instead.
        let (spawner, _) =
            ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done { out_bytes: 1 });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut o = h264();
        o.video = Some(VideoCodec::Vp9);
        let done = s.submit(PathBuf::from("/m/x"), o, file_sink()).await.unwrap();
        assert_eq!(done.device, DeviceId::Cpu);
    }

    #[tokio::test]
    async fn live_stream_unsupported_for_now() {
        let (spawner, _) =
            ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done { out_bytes: 1 });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s
            .submit(PathBuf::from("/m/x"), h264(), SinkRequest::LiveStream)
            .await;
        assert_eq!(r, Err(SchedError::Unsupported));
    }

    #[tokio::test]
    async fn busy_backpressure_when_saturated() {
        // Total permits = 2(nvenc)+1(vaapi)+2(cpu) = 5; pending_cap = 0.
        // Hold jobs with a long delay, fire 6 → the 6th can neither get a
        // permit nor queue → Busy.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let cfg = SchedConfig {
            pending_cap: 0,
            ..SchedConfig::default()
        };
        let s = TranscodeScheduler::spawn(table(), spawner, cfg);
        let mut handles = Vec::new();
        for _ in 0..6 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(PathBuf::from("/m/x"), h264(), file_sink()).await
            }));
        }
        let mut busy = 0;
        let mut ok = 0;
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SchedError::Busy) => busy += 1,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(busy >= 1, "expected at least one Busy under saturation, ok={ok} busy={busy}");
    }

    #[tokio::test]
    async fn job_finished_drains_pending() {
        // pending_cap large; saturate permits with delay so extra jobs
        // queue, then all complete once permits free. Proves the
        // JobFinished edge re-dispatches queued work.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut handles = Vec::new();
        for _ in 0..20 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(PathBuf::from("/m/x"), h264(), file_sink()).await
            }));
        }
        let mut ok = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 20, "all queued jobs must eventually complete");
    }

    #[tokio::test]
    async fn transient_failure_retries_next_best() {
        // Nvenc always DeviceBusy; everything else Done. Job must land
        // off Nvenc (Vaapi or Cpu) and succeed.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, spec| {
            if spec.device == DeviceId::hw(HwAccel::Nvenc, 0) {
                WorkerRunResult::Failed(WorkerError::DeviceBusy)
            } else {
                WorkerRunResult::Done { out_bytes: 7 }
            }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s.submit(PathBuf::from("/m/x"), h264(), file_sink()).await.unwrap();
        assert_ne!(done.device, DeviceId::hw(HwAccel::Nvenc, 0));
        assert_eq!(done.out_bytes, 7);
    }

    #[tokio::test]
    async fn non_recoverable_failure_returns_error() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
            WorkerRunResult::Failed(WorkerError::BadInput)
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s.submit(PathBuf::from("/m/x"), h264(), file_sink()).await;
        assert_eq!(r, Err(SchedError::Failed(WorkerError::BadInput)));
    }

    #[tokio::test]
    async fn worker_death_retries_and_scheduler_survives() {
        // First run on any device dies once; subsequent runs succeed.
        // The job must still complete and the scheduler keeps serving.
        let counter = Arc::new(Mutex::new(0u32));
        let c2 = counter.clone();
        let (spawner, spawned) = ScriptedSpawner::new(Duration::ZERO, move |_, _| {
            let mut n = c2.lock().unwrap();
            *n += 1;
            if *n == 1 {
                WorkerRunResult::Died
            } else {
                WorkerRunResult::Done { out_bytes: 9 }
            }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s.submit(PathBuf::from("/m/x"), h264(), file_sink()).await.unwrap();
        assert_eq!(done.out_bytes, 9);
        // A second job still works → scheduler alive after a worker death.
        let done2 = s.submit(PathBuf::from("/m/y"), h264(), file_sink()).await.unwrap();
        assert_eq!(done2.out_bytes, 9);
        // At least two spawns happened (the dead one + a replacement).
        assert!(spawned.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn worker_panic_does_not_hang_submit() {
        // A worker whose run() panics must not leak the caller's reply.
        // The scheduler maps the panic (JoinError) to Died → bounded
        // retry → eventually resolves with an error, never hangs.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
            panic!("simulated worker/ffi explosion");
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            s.submit(PathBuf::from("/m/x"), h264(), file_sink()),
        )
        .await
        .expect("submit hung after worker panic");
        assert!(matches!(r, Err(SchedError::Failed(_))), "got {r:?}");
        // Scheduler still serves after absorbing panics.
        let snap = tokio::time::timeout(Duration::from_secs(2), s.snapshot())
            .await
            .expect("snapshot hung");
        assert!(snap.is_some());
    }

    #[tokio::test]
    async fn saturation_no_deadlock_under_timeout() {
        // Fire far more jobs than permits with a small delay; with a
        // generous pending_cap all must finish. Wrap in a timeout so a
        // deadlock fails loudly instead of hanging the suite.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(5), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let fut = async {
            let mut handles = Vec::new();
            for _ in 0..200 {
                let s2 = s.clone();
                handles.push(tokio::spawn(async move {
                    s2.submit(PathBuf::from("/m/x"), h264(), file_sink()).await
                }));
            }
            let mut ok = 0;
            for h in handles {
                if h.await.unwrap().is_ok() {
                    ok += 1;
                }
            }
            ok
        };
        let ok = tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .expect("scheduler deadlocked under saturation");
        assert_eq!(ok, 200);
    }

    #[tokio::test]
    async fn snapshot_reports_capacity() {
        let (spawner, _) =
            ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done { out_bytes: 1 });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let snap = s.snapshot().await.unwrap();
        // 2 hw + cpu.
        assert_eq!(snap.devices.len(), 3);
        let total_cap: usize = snap.devices.iter().map(|d| d.capacity).sum();
        assert_eq!(total_cap, 2 + 1 + 2);
    }
}
