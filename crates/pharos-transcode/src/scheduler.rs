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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

/// Who is waiting on a job.
///
/// The scheduler's pending queue is shared by client requests and speculative
/// warm-up, and until this existed the two were indistinguishable in every
/// signal the server emitted: a segment a browser was blocked on and a segment
/// nobody had asked for queued identically. "Why did this segment wait 90 s?"
/// was therefore unanswerable — the wait was visible, what it waited behind
/// was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobClass {
    /// A client response is blocked on this job completing.
    Interactive,
    /// Speculative warm-up (prefetch, seek prewarm). Nobody is waiting;
    /// arriving late is worthless and arriving never is cheap.
    Background,
}

impl JobClass {
    /// Stable metric-label string. Bounded cardinality, and a rename here
    /// breaks dashboards silently — pinned by a test.
    pub fn label(self) -> &'static str {
        match self {
            JobClass::Interactive => "interactive",
            JobClass::Background => "background",
        }
    }
}

/// What happened when a shared-init fMP4 rendition was resolved to its encoder.
///
/// Spec 003 pins such a rendition to ONE device because its segments all decode
/// under a single init (V80); `pharos_transcode_pin_total{outcome}` is how that
/// guarantee is observed rather than merely asserted. The three variants are
/// exhaustive over the shared-init path, so they sum to the number of
/// shared-init jobs placed — a bucket that stops adding up means a branch is
/// resolving devices without saying so.
///
/// `Invalidated` is the one to alert on: it means a rendition's device went
/// unavailable mid-stream and the request was FAILED rather than spilled onto
/// a second encoder, which is a visible stall for the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// The rendition resolved to an eligible device and was placed on it.
    Followed,
    /// The resolved device was not eligible (cooldown / excluded), so the
    /// request failed rather than mixing encoders under one init (#114).
    Invalidated,
    /// No device could be resolved for the rendition at all; placement falls
    /// through to the normal load-balanced path.
    Unresolved,
}

impl PinOutcome {
    /// Stable metric-label string. Same contract as [`JobClass::label`]: these
    /// appear as the `outcome` label on `pharos_transcode_pin_total`, so a
    /// rename breaks dashboards silently and a COLLISION is worse still —
    /// `invalidated` folded into `followed` would report a broken pin as a
    /// healthy one. Pinned by a test.
    pub fn label(self) -> &'static str {
        match self {
            PinOutcome::Followed => "followed",
            PinOutcome::Invalidated => "invalidated",
            PinOutcome::Unresolved => "unresolved",
        }
    }

    /// Record this outcome. One call per shared-init placement decision.
    fn record(self) {
        metrics::counter!("pharos_transcode_pin_total", "outcome" => self.label()).increment(1);
    }
}

impl std::fmt::Display for JobClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A live transcode output as a stream of muxed byte chunks. Boxed so the
/// type stays platform-agnostic (the concrete unix worker stream lives in
/// `worker::proc`). The stream owns the worker process + its device
/// permit; dropping it tears the encode down (broken pipe → ffmpeg exits)
/// and frees the slot.
pub type LiveByteStream = Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Terminal result of one worker running one job.
#[derive(Debug)]
pub enum WorkerRunResult {
    Done {
        out_bytes: u64,
    },
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

impl SinkRequest {
    /// Bounded label for the `sink` span field. Stable strings: a dashboard
    /// keyed on these breaks silently if renamed.
    pub fn label(&self) -> &'static str {
        match self {
            Self::FileDirect { .. } => "file",
            Self::LiveStream => "live_stream",
        }
    }

    /// Whether this sink's byte count is something the worker can actually
    /// measure. A live stream's bytes go straight down the pipe to the main
    /// process, so the worker reports `0` — which is indistinguishable from
    /// "produced nothing" in a log, and read as exactly that during the Ghost
    /// in the Shell investigation (two jobs looked like silent empty encodes;
    /// they were healthy live streams).
    pub fn measures_out_bytes(&self) -> bool {
        matches!(self, Self::FileDirect { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub device: DeviceId,
    pub out_bytes: u64,
    /// Time the job spent queued before its (final) dispatch — includes any
    /// failed-device retry churn. High = saturated devices / retries.
    pub queue_wait_ms: u64,
    /// Time the winning device took to actually encode. High = slow encoder.
    pub encode_ms: u64,
    /// How many OTHER jobs were already running on the same device when this
    /// one was dispatched. `encode_ms` says a segment was slow; this says
    /// whether it was slow or merely crowded, which is the difference between
    /// "the encoder is too slow for this source" and "we scheduled six
    /// speculative encodes on top of it". Measured on the deployment: 1 860 ms
    /// alone, 6 229 ms with six peers.
    pub peer_jobs: usize,
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
    /// Queue depth split by who is waiting. A backlog that is almost all
    /// `background` is a queue of work nobody asked for, sitting in front of
    /// the segments a client is blocked on — the shape that turns a 3 s encode
    /// into a 90 s wait.
    pub pending_interactive: usize,
    pub pending_background: usize,
    /// How long the oldest queued job has been waiting. `pending` says the
    /// queue is deep; this says whether anything is actually stuck in it.
    pub oldest_pending_ms: Option<u64>,
    /// Jobs holding a device permit via the segment path (they report a
    /// `JobFinished`).
    pub inflight: usize,
    /// Streams holding a device permit via [`TranscodeScheduler::submit_live`].
    /// These never enter `inflight` and never report a `JobFinished`, so a
    /// permit held for the lifetime of a progressive transcode was previously
    /// invisible: `in_use` was occupied with nothing to attribute it to.
    pub live_streams: usize,
}

#[derive(Debug, Clone)]
pub struct DeviceStat {
    pub id: DeviceId,
    pub capacity: usize,
    pub in_use: usize,
    pub in_cooldown: bool,
    /// Occupancy attributed to segment jobs, split by class. `in_use` minus
    /// these (minus any live stream on this device) is unattributed occupancy.
    pub inflight_interactive: usize,
    pub inflight_background: usize,
}

/// Tunables.
#[derive(Debug, Clone)]
pub struct SchedConfig {
    pub inbox_depth: usize,
    pub pending_cap: usize,
    pub cooldown: Duration,
    pub max_retries: u8,
    /// Permits kept out of reach of [`JobClass::Background`] work. A
    /// speculative job is admitted only while more than this many permits are
    /// free across the devices that could take it, so a burst of prefetch can
    /// never occupy the last slot a client request would have used.
    pub background_headroom: usize,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            inbox_depth: 256,
            pending_cap: 256,
            cooldown: Duration::from_secs(2),
            max_retries: 3,
            background_headroom: 1,
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
        class: JobClass,
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
    /// When the job entered the scheduler (first `Submit`). Used to split
    /// end-to-end latency into queue-wait vs actual encode: the segment-path
    /// `transcode_ms` conflates the two, hiding whether a slow segment is a
    /// saturated device (long queue) or a slow encoder.
    enqueued: Instant,
    /// When the job most recently grabbed a device permit + started running.
    /// `None` while queued. Re-stamped on each (re)dispatch so a retry's wait
    /// is counted in the queue, not the encode.
    dispatched: Option<Instant>,
    /// Who is waiting. Carried through retries + requeues so a job's class is
    /// the same wherever it is observed (queued, inflight, finished).
    class: JobClass,
    /// Device this job is currently running on. `None` while queued. Lets a
    /// snapshot attribute each device's occupancy to the jobs holding it,
    /// instead of reporting a bare count with nothing behind it.
    device: Option<DeviceId>,
    /// Jobs already on that device at the moment this one was dispatched.
    /// Re-stamped on each (re)dispatch, like `dispatched`, so a retry reports
    /// the company it actually kept rather than the company it first met.
    peer_jobs: usize,
    /// The job's own span, opened at dispatch (when the device — and so the
    /// decode path — is finally known) and carried here so the completion
    /// arm, which runs in the ACTOR and not inside the job's task, can still
    /// record the outcome on it. Every event the job emits, and every trace
    /// exported to Tempo, carries the placement facts as a result: a wedged
    /// segment can be read without joining three log lines by job id.
    span: tracing::Span,
}

struct SchedState {
    devices: DeviceTable,
    spawner: Arc<dyn WorkerSpawner>,
    idle: Vec<Box<dyn Worker>>,
    inflight: HashMap<JobId, JobCtx>,
    pending: VecDeque<(JobId, JobCtx)>,
    cfg: SchedConfig,
    /// Live streams currently holding a permit. Shared with each
    /// [`PermitStream`], which decrements on drop — the only bookkeeping the
    /// live path has, since it reports no `JobFinished`.
    live: Arc<AtomicUsize>,
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
            live: Arc::new(AtomicUsize::new(0)),
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
        class: JobClass,
    ) -> Result<JobDone, SchedError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::Submit {
                input,
                opts,
                sink,
                class,
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
    job_id: JobId,
    device: DeviceId,
    started: Instant,
    live: Arc<AtomicUsize>,
}

impl futures_core::Stream for PermitStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for PermitStream {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
        // Closes the pair with the acquisition log. A device sitting at
        // capacity with no segment jobs to account for it is a live stream
        // holding a permit; without a release line there is no way to tell a
        // stream that ended from one still running.
        tracing::info!(
            job_id = %self.job_id,
            device = %self.device,
            held_ms = self.started.elapsed().as_millis() as u64,
            "live transcode released its device permit"
        );
    }
}

fn handle(state: &mut SchedState, msg: SchedMsg, self_tx: &mpsc::Sender<SchedMsg>) {
    match msg {
        SchedMsg::Submit {
            input,
            opts,
            sink,
            class,
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
                enqueued: Instant::now(),
                dispatched: None,
                class,
                device: None,
                peer_jobs: 0,
                // Replaced at dispatch, once the device is known.
                span: tracing::Span::none(),
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
                // The live path has no queue: a rejected stream falls back to
                // an *unscheduled* inline ffmpeg at the caller, which holds no
                // permit and is therefore invisible to every saturation gauge.
                // A silent rejection here is the start of load the scheduler
                // cannot see, so it is never silent.
                let busy = !eligible.is_empty();
                tracing::warn!(
                    input = %input.display(),
                    eligible = ?eligible,
                    occupancy = ?state
                        .devices
                        .slots()
                        .iter()
                        .map(|s| (s.id, s.in_use(), s.capacity))
                        .collect::<Vec<_>>(),
                    reason = if busy { "all permits busy" } else { "no eligible device" },
                    "live transcode rejected; caller falls back off-scheduler"
                );
                let _ = reply.send(Err(if busy {
                    SchedError::Busy
                } else {
                    SchedError::Unsupported
                }));
                return;
            };
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            // A live stream holds its permit for as long as the client keeps
            // reading — minutes, not the seconds a segment takes. Name the
            // device it took at acquisition; `PermitStream::drop` closes the
            // pair with how long it held it.
            tracing::info!(
                %job_id,
                device = %device,
                eligible = ?eligible,
                input = %input.display(),
                "live transcode took a device permit"
            );
            let live = state.live.clone();
            live.fetch_add(1, Ordering::Relaxed);
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
                            job_id,
                            device,
                            started: Instant::now(),
                            live,
                        });
                        let _ = reply.send(Ok(stream));
                    }
                    Err(e) => {
                        drop(permit);
                        live.fetch_sub(1, Ordering::Relaxed);
                        tracing::warn!(%job_id, device = %device, error = %e, "live transcode spawn failed; permit released");
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
                    // Split end-to-end latency: queue-wait (Submit → first
                    // dispatch, includes any failed-device retries) vs encode
                    // (last dispatch → now). A slow segment with high
                    // queue_wait_ms = saturated devices / retry churn; high
                    // encode_ms = a genuinely slow encoder. `retries` > 0 flags
                    // a job that bounced off a device (e.g. a phantom GPU).
                    let now = Instant::now();
                    let queue_ms = ctx
                        .dispatched
                        .map(|d| d.saturating_duration_since(ctx.enqueued).as_millis() as u64)
                        .unwrap_or(0);
                    let encode_ms = ctx
                        .dispatched
                        .map(|d| now.saturating_duration_since(d).as_millis() as u64)
                        .unwrap_or(0);
                    // Close the loop on the span opened at dispatch, so the
                    // trace carries the outcome next to the placement that
                    // produced it rather than in a separate line to be joined
                    // by job id.
                    ctx.span.record("queue_wait_ms", queue_ms);
                    ctx.span.record("encode_ms", encode_ms);
                    // Only record a byte count the worker could actually
                    // measure: a live-stream job's bytes go down the pipe, so
                    // its `0` means "not measured", not "produced nothing".
                    // Leaving the field ABSENT says that; writing 0 lies.
                    if ctx.sink.measures_out_bytes() {
                        ctx.span.record("out_bytes", out_bytes);
                    }
                    ctx.span.record("outcome", "done");
                    ctx.span.in_scope(|| {
                        tracing::info!(
                            %job_id,
                            %device,
                            out_bytes,
                            sink = ctx.sink.label(),
                            class = %ctx.class,
                            queue_wait_ms = queue_ms,
                            encode_ms,
                            peer_jobs = ctx.peer_jobs,
                            retries = ctx.retries,
                            "transcode job done"
                        );
                    });
                    let _ = ctx.reply.send(Ok(JobDone {
                        device,
                        out_bytes,
                        queue_wait_ms: queue_ms,
                        encode_ms,
                        peer_jobs: ctx.peer_jobs,
                    }));
                }
                WorkerRunResult::Failed(err) if !err.is_transient() => {
                    // Symmetry: whatever the success path records, the failure
                    // path records too. A span that only ever carries an
                    // outcome when the job succeeded is the shape that hides
                    // outages.
                    ctx.span.record("outcome", "failed");
                    ctx.span.in_scope(|| {
                        tracing::warn!(%job_id, %device, error = %err, "transcode job failed (non-recoverable)");
                    });
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
                    ctx.span.record("outcome", "transient_retry");
                    ctx.retries += 1;
                    ctx.last_error = Some(err);
                    retry_or_fail(state, job_id, ctx, self_tx);
                }
                WorkerRunResult::Died => {
                    // Worker death is not the device's fault — don't cool
                    // the device, but count the retry and re-place. A
                    // fresh worker is spawned on the next dispatch.
                    ctx.span.record("outcome", "worker_died");
                    ctx.span.in_scope(|| {
                        tracing::warn!(%job_id, %device, "transcode worker died mid-job; retrying");
                    });
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
                    inflight_interactive: state
                        .inflight
                        .values()
                        .filter(|c| c.class == JobClass::Interactive && c.device == Some(s.id))
                        .count(),
                    inflight_background: state
                        .inflight
                        .values()
                        .filter(|c| c.class == JobClass::Background && c.device == Some(s.id))
                        .count(),
                })
                .collect();
            let now = Instant::now();
            let _ = reply.send(SchedSnapshot {
                devices,
                pending: state.pending.len(),
                idle_workers: state.idle.len(),
                pending_interactive: state
                    .pending
                    .iter()
                    .filter(|(_, c)| c.class == JobClass::Interactive)
                    .count(),
                pending_background: state
                    .pending
                    .iter()
                    .filter(|(_, c)| c.class == JobClass::Background)
                    .count(),
                oldest_pending_ms: state
                    .pending
                    .iter()
                    .map(|(_, c)| now.saturating_duration_since(c.enqueued).as_millis() as u64)
                    .max(),
                inflight: state.inflight.len(),
                live_streams: state.live.load(Ordering::Relaxed),
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

/// Segment jobs already running on `dev`. This is what sets encode time on a
/// shared encoder — a device at capacity finishes each of its jobs several
/// times slower than the same device running one — and nothing recorded it, so
/// a slow segment and a crowded one produced identical telemetry.
fn peers_on(state: &SchedState, dev: DeviceId) -> usize {
    state
        .inflight
        .values()
        .filter(|c| c.device == Some(dev))
        .count()
}

/// Permits currently free across the devices that could take a given job.
/// Counted over the job's own candidate set, not the whole table: capacity on
/// a device that cannot encode this target is not headroom for it.
fn free_permits(state: &SchedState, candidates: &[DeviceId]) -> usize {
    candidates
        .iter()
        .filter_map(|d| state.devices.slot(*d))
        .map(|s| s.capacity.saturating_sub(s.in_use()))
        .sum()
}

/// A job that waited longer than this before starting is reported with the
/// state of the queue it just escaped. One segment covers 6 s of playback, so
/// a wait past this is already eating a client's buffer.
const LONG_WAIT_MS: u64 = 3_000;

/// Report a job that queued for a long time *together with what it queued
/// behind*. `queue_wait_ms` on the finished job says a segment waited; only
/// the composition of the queue at the moment it was finally dispatched says
/// whether it waited behind other client requests (genuine overload) or behind
/// speculative warm-up nobody was waiting for (a scheduling defect).
fn warn_if_long_wait(
    state: &SchedState,
    job_id: JobId,
    device: DeviceId,
    ctx: &JobCtx,
    dispatched_at: Instant,
) {
    let waited_ms = dispatched_at
        .saturating_duration_since(ctx.enqueued)
        .as_millis() as u64;
    if waited_ms < LONG_WAIT_MS {
        return;
    }
    let (mut pend_i, mut pend_b) = (0usize, 0usize);
    for (_, c) in state.pending.iter() {
        match c.class {
            JobClass::Interactive => pend_i += 1,
            JobClass::Background => pend_b += 1,
        }
    }
    let (mut run_i, mut run_b) = (0usize, 0usize);
    for c in state.inflight.values() {
        match c.class {
            JobClass::Interactive => run_i += 1,
            JobClass::Background => run_b += 1,
        }
    }
    tracing::warn!(
        %job_id,
        %device,
        class = %ctx.class,
        waited_ms,
        retries = ctx.retries,
        pending_interactive = pend_i,
        pending_background = pend_b,
        inflight_interactive = run_i,
        inflight_background = run_b,
        live_streams = state.live.load(Ordering::Relaxed),
        input = %ctx.input.display(),
        "transcode job waited a long time for a device permit"
    );
}

/// Open the job's span and record where it was placed — including whether its
/// SOURCE DECODE landed on the GPU.
///
/// Both dispatch paths (first attempt and a queued job draining onto a freed
/// permit) go through here, so a job that waited is described exactly like one
/// that did not.
///
/// The decode verdict comes from the same [`crate::DecodeAccel`] the argv
/// builder emits `-hwaccel` from, so what the dashboard reports and what
/// ffmpeg is told are one decision. Before this, the only thing recorded was
/// the DEVICE — and a job on `Nvenc:0` may still decode every frame in
/// software, which is indistinguishable in a log that names the device alone.
///
/// At INFO because that is the level the deployment runs at. The line naming
/// the winning device sat at DEBUG while prod placed 60% of its segments on
/// the CPU, so the record that would have said so was never emitted.
fn record_placement(
    job_id: JobId,
    dev: DeviceId,
    ctx: &JobCtx,
    candidates: &[DeviceId],
) -> tracing::Span {
    let decode = crate::DecodeAccel::of(&ctx.opts, dev);
    // `queue_wait_ms`/`encode_ms`/`out_bytes`/`outcome` are declared empty and
    // recorded when the job finishes, so one span carries the whole life of a
    // segment: what it was, where it went, how long it waited, how it ended.
    let span = tracing::info_span!(
        "transcode_job",
        job_id = %job_id,
        device = %dev,
        class = %ctx.class,
        sink = ctx.sink.label(),
        decode_accel = %decode,
        decode_on_gpu = decode.is_gpu(),
        video = ?ctx.opts.video,
        audio = ?ctx.opts.audio,
        container = ?ctx.opts.container,
        seek_secs = ctx.opts.start_position_ticks as f64 / 10_000_000.0,
        dur_secs = ctx.opts.duration_ticks.map(|t| t as f64 / 10_000_000.0),
        retries = ctx.retries,
        peer_jobs = tracing::field::Empty,
        queue_wait_ms = tracing::field::Empty,
        encode_ms = tracing::field::Empty,
        out_bytes = tracing::field::Empty,
        outcome = tracing::field::Empty,
        input = %ctx.input.display(),
    );
    // Which device won and what it beat. A deployment silently serving
    // everything from the CPU — because the GPU is in cooldown or was excluded
    // by an earlier transient failure — is indistinguishable from one with no
    // GPU at all unless the losing candidates are named alongside the winner.
    span.in_scope(|| {
        tracing::info!(
            candidates = ?candidates,
            excluded = ?ctx.excluded,
            "transcode dispatch"
        );
    });
    metrics::counter!(
        "pharos_transcode_decode_accel_total",
        "verdict" => decode.label(),
        "device" => dev.to_string(),
        "class" => ctx.class.label(),
    )
    .increment(1);
    span
}

/// Try to dispatch `ctx` to its best eligible device; queue if all
/// permits are busy; fail if no device can ever take it.
fn place(state: &mut SchedState, job_id: JobId, mut ctx: JobCtx, self_tx: &mpsc::Sender<SchedMsg>) {
    // Caller gone (client seeked/disconnected → dropped the `submit().await`
    // and its oneshot receiver): don't spend a worker on a segment nobody is
    // waiting for. This is the post-seek contention fix — a dead prefetch job
    // must not sit ahead of the seek-target segment in a device queue.
    if ctx.reply.is_closed() {
        return;
    }
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    if full_eligible.is_empty() {
        // No supporting device at all (e.g. cooldown could hide all HW
        // but CPU always supports; truly empty ⇒ unsupported target).
        let _ = ctx.reply.send(Err(SchedError::Unsupported));
        return;
    }
    // Spec 003 — a shared-init fMP4 rendition must come from ONE encoder, so it
    // does not get a choice of devices. The device is a pure function of the
    // rendition (see `DeviceTable::rendition_device`), which keeps the answer
    // stable across a restart; an in-memory pin would not, and a rendition
    // re-pinned mid-playback serves segments that no longer match the client's
    // init (issue #114 — undecodable video, served with a 200).
    //
    // Cooldown deliberately does NOT re-route it. Spilling to a second encoder
    // is exactly the failure this prevents, so an unavailable device FAILS the
    // request instead: the client restarts the stream and re-fetches an init
    // that matches whatever produces it next. A visible stall that recovers
    // beats silent corruption.
    let pinned = if crate::device::shared_init_fmp4(&ctx.opts) {
        let key = crate::options::RenditionKey::new(&ctx.input, &ctx.opts);
        match state.devices.rendition_device(&ctx.opts, key.value()) {
            Some(d) => {
                if !full_eligible.contains(&d) {
                    PinOutcome::Invalidated.record();
                    tracing::warn!(
                        %job_id,
                        rendition = %key.short(),
                        device = %d,
                        "rendition device unavailable (cooldown); failing rather than spilling to another encoder"
                    );
                    let _ = ctx
                        .reply
                        .send(Err(SchedError::Failed(WorkerError::Other(format!(
                        "rendition device {d} unavailable; refusing to mix encoders under one init"
                    )))));
                    return;
                }
                PinOutcome::Followed.record();
                Some(d)
            }
            None => {
                // Previously silent. Without it the counter cannot be read as a
                // total: `followed + invalidated` was always short by however
                // many shared-init jobs resolved to no device, and there was no
                // way to tell that from the metric.
                PinOutcome::Unresolved.record();
                None
            }
        }
    } else {
        None
    };

    // Candidate devices = eligible minus already-tried. A pinned rendition has
    // exactly one candidate and never widens.
    let candidates: SmallVec<[DeviceId; 5]> = match pinned {
        Some(d) => full_eligible
            .iter()
            .copied()
            .filter(|c| *c == d && !ctx.excluded.contains(c))
            .collect(),
        None => full_eligible
            .iter()
            .copied()
            .filter(|d| !ctx.excluded.contains(d))
            .collect(),
    };
    if candidates.is_empty() {
        // Every supporting device has been tried + failed transiently.
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        tracing::warn!(
            %job_id,
            excluded = ?ctx.excluded,
            error = %err,
            "transcode job has no device left to try"
        );
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }

    // Speculative work waits for nobody, so it must not make anybody wait.
    // Prefetch is dispatched *before* the segment the client is blocked on
    // (it pipelines, by design), and shared one FIFO with that request: a
    // handful of requests could therefore bury a client's own segment under
    // tens of speculative encodes, turning a 3 s encode into a 90 s wait.
    // Background work now runs only out of genuine spare capacity — it is
    // shed the moment taking a permit would eat into the reserve, and it
    // never enters the queue at all.
    if ctx.class == JobClass::Background
        && free_permits(state, &candidates) <= state.cfg.background_headroom
    {
        tracing::debug!(
            %job_id,
            candidates = ?candidates,
            headroom = state.cfg.background_headroom,
            "speculative transcode shed: no spare capacity"
        );
        let _ = ctx.reply.send(Err(SchedError::Busy));
        return;
    }

    for dev in candidates.iter().copied() {
        let Some(slot) = state.devices.slot(dev) else {
            continue;
        };
        if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
            let span = record_placement(job_id, dev, &ctx, &candidates);
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
            let dispatched_at = Instant::now();
            // The long-wait warning belongs INSIDE the span: the queue
            // composition it reports is only actionable beside what the job
            // was and where it landed.
            span.in_scope(|| warn_if_long_wait(state, job_id, dev, &ctx, dispatched_at));
            ctx.dispatched = Some(dispatched_at);
            ctx.device = Some(dev);
            // Counted BEFORE this job joins `inflight`, so it is peers, not
            // occupancy: a job that runs alone reports 0.
            ctx.peer_jobs = peers_on(state, dev);
            span.record("peer_jobs", ctx.peer_jobs);
            ctx.span = span.clone();
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
                span,
            );
            return;
        }
    }

    // All candidate permits busy → queue (or backpressure). Background work
    // never queues: by the time a permit frees, the client has usually asked
    // for the segment itself, and a queued speculative job holds the cache's
    // per-key fetch lock — so that client request would inherit the whole wait
    // it was meant to be spared.
    if ctx.class == JobClass::Background {
        let _ = ctx.reply.send(Err(SchedError::Busy));
        return;
    }
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
    mut ctx: JobCtx,
    self_tx: &mpsc::Sender<SchedMsg>,
    requeue: &mut VecDeque<(JobId, JobCtx)>,
) {
    // Drop a queued job whose caller has gone (see `place`): on a freed permit
    // we dispatch the seek-target instead of resurrecting dead prefetch work.
    if ctx.reply.is_closed() {
        return;
    }
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
            let span = record_placement(job_id, dev, &ctx, &candidates);
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
            let dispatched_at = Instant::now();
            // The long-wait warning belongs INSIDE the span: the queue
            // composition it reports is only actionable beside what the job
            // was and where it landed.
            span.in_scope(|| warn_if_long_wait(state, job_id, dev, &ctx, dispatched_at));
            ctx.dispatched = Some(dispatched_at);
            ctx.device = Some(dev);
            ctx.span = span.clone();
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
                span,
            );
            return;
        }
    }
    // Background work never enters `pending` (see `place`), so this can only
    // be reached by a job that was queued as interactive. Re-queue it.
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
    // The job's span, so a worker-spawn failure or a mid-job death is
    // reported with the placement facts attached rather than a bare job id.
    span: tracing::Span,
) {
    let job_id = spec.job_id;
    tokio::spawn(async move {
        let _guard = span.enter();
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

    #[test]
    fn only_a_file_sink_claims_to_have_measured_its_output() {
        // `out_bytes: 0` from a live-stream job means "the worker never saw the
        // bytes", not "the encode produced nothing". Conflating them cost a
        // wrong lead during the Ghost in the Shell investigation.
        let file = SinkRequest::FileDirect {
            out_path: PathBuf::from("/tmp/x.ts"),
        };
        assert!(file.measures_out_bytes());
        assert!(!SinkRequest::LiveStream.measures_out_bytes());
        assert_eq!(file.label(), "file");
        assert_eq!(SinkRequest::LiveStream.label(), "live_stream");
        assert_ne!(file.label(), SinkRequest::LiveStream.label());
    }

    fn h264() -> TranscodeOptions {
        TranscodeOptions {
            source_frame_rate: None,
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
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
        fn spawn(&self, id: WorkerId) -> SpawnFuture {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let f = self.f.clone();
            let delay = self.delay;
            Box::pin(
                async move { Ok(Box::new(ScriptedWorker { id, f, delay }) as Box<dyn Worker>) },
            )
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
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 42,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        assert_eq!(done.device, DeviceId::hw(HwAccel::Nvenc, 0)); // best-first
        assert_eq!(done.out_bytes, 42);
    }

    fn cmaf() -> TranscodeOptions {
        let mut o = h264();
        o.container = Container::Fmp4;
        o
    }

    /// Spec 003 US2 — every segment of one CMAF rendition must come from the
    /// SAME device. Dispatch many segments (they differ only in start position,
    /// so they share a rendition key) and assert they all land together, with
    /// other devices free to tempt the load-balancer.
    ///
    /// If this fails, issue #114 is back: a segment from a second encoder is
    /// undecodable under the init the client already holds.
    #[tokio::test]
    async fn every_segment_of_a_cmaf_rendition_lands_on_one_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut devices = std::collections::HashSet::new();
        for seg in 0..12u64 {
            let mut o = cmaf();
            o.start_position_ticks = seg * 6 * 10_000_000;
            o.duration_ticks = Some(6 * 10_000_000);
            let done = s
                .submit(
                    PathBuf::from("/m/show.mkv"),
                    o,
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
                .unwrap();
            devices.insert(done.device);
        }
        assert_eq!(
            devices.len(),
            1,
            "a shared-init rendition must not mix encoders; got {devices:?}"
        );
    }

    /// A DIFFERENT rendition of the same file (different audio track) is a
    /// different init, so it is free to resolve elsewhere — the guarantee is
    /// per-rendition, not per-file.
    #[tokio::test]
    async fn a_different_rendition_is_free_to_choose_again() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let a = s
            .submit(
                PathBuf::from("/m/x.mkv"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        let mut other = cmaf();
        other.audio_source_stream_index = Some(3);
        let b = s
            .submit(
                PathBuf::from("/m/x.mkv"),
                other,
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        // Both are valid; the point is that each is internally consistent and
        // neither is forced to follow the other.
        assert!(matches!(a.device, DeviceId::Hw { .. }));
        assert!(matches!(b.device, DeviceId::Hw { .. }));
    }

    /// Spec 003 US3 — losing the rendition's device must FAIL, not fall back.
    ///
    /// Falling back is precisely issue #114: the client keeps an init produced
    /// by the first encoder and would silently receive segments from a second.
    /// A visible error makes the player restart the stream and re-fetch an init
    /// that matches. CPU is left deliberately free here, so a spill would
    /// succeed if the guard were absent.
    ///
    /// Disarm by deleting the `!full_eligible.contains(&d)` arm in `place()`
    /// and this goes red — the job completes on CPU.
    #[tokio::test]
    async fn a_cooled_rendition_device_fails_instead_of_spilling_to_another_encoder() {
        let mut t = table();
        let dev = t
            .rendition_device(&cmaf(), {
                use crate::options::RenditionKey;
                RenditionKey::new(std::path::Path::new("/m/show.mkv"), &cmaf()).value()
            })
            .expect("a device");
        assert!(matches!(dev, DeviceId::Hw { .. }), "precondition: hardware");
        t.set_cooldown(dev, Instant::now() + Duration::from_secs(300));

        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(t, spawner, SchedConfig::default());
        let res = s
            .submit(
                PathBuf::from("/m/show.mkv"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
            )
            .await;
        match res {
            Err(SchedError::Failed(_)) => {}
            other => panic!(
                "a cooled rendition device must fail, not silently re-encode elsewhere; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn vp9_lands_on_vaapi_hardware() {
        // VAAPI has a VP9 encoder (`vp9_vaapi`); the scheduler routes a VP9 job
        // to it (hardware) rather than the CPU. NVENC has no VP9 encoder so it is
        // never eligible for a VP9 target.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut o = h264();
        o.video = Some(VideoCodec::Vp9);
        let done = s
            .submit(PathBuf::from("/m/x"), o, file_sink(), JobClass::Interactive)
            .await
            .unwrap();
        assert_eq!(done.device, DeviceId::hw(HwAccel::Vaapi, 0));
    }

    #[tokio::test]
    async fn live_stream_unsupported_for_now() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                SinkRequest::LiveStream,
                JobClass::Interactive,
            )
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
                s2.submit(
                    PathBuf::from("/m/x"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
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
        assert!(
            busy >= 1,
            "expected at least one Busy under saturation, ok={ok} busy={busy}"
        );
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
                s2.submit(
                    PathBuf::from("/m/x"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
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
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        assert_ne!(done.device, DeviceId::hw(HwAccel::Nvenc, 0));
        assert_eq!(done.out_bytes, 7);
    }

    /// Collects the fields of every event, so a test can assert what the
    /// deployment would actually be able to read.
    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<(String, String)>>>);

    impl<S> tracing_subscriber::Layer<S> for EventCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visit(String);
            impl tracing::field::Visit for Visit {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!(" {}={:?}", f.name(), v));
                }
            }
            let mut v = Visit(String::new());
            event.record(&mut v);
            // Span fields are the point of the exercise: an event is only as
            // debuggable as the span it was emitted inside.
            let mut fields = v.0;
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope {
                    if let Some(f) = span
                        .extensions()
                        .get::<tracing_subscriber::fmt::FormattedFields<
                            tracing_subscriber::fmt::format::DefaultFields,
                        >>()
                    {
                        fields.push_str(&format!(" [{}: {}]", span.name(), f.fields));
                    }
                }
            }
            self.0
                .lock()
                .unwrap()
                .push((event.metadata().name().to_string(), fields));
        }
    }

    /// The placement record is a contract, not a convenience: it is the ONLY
    /// thing that can answer "is this deployment decoding on the GPU?".
    ///
    /// A production incident turned on exactly this gap — 60% of segments were
    /// placed on the CPU while the line naming the winning device sat at
    /// DEBUG, so the deployment's own logs could not distinguish a working GPU
    /// from an idle one. Asserting the fields (not just that something was
    /// logged) is what makes a later refactor unable to quietly drop them.
    #[tokio::test]
    async fn a_dispatch_records_its_device_and_whether_decode_is_on_the_gpu() {
        use tracing_subscriber::layer::SubscriberExt;

        let cap = EventCapture::default();
        // INFO, because that is what the deployment runs at. Without this
        // filter the test would pass with the record at DEBUG — which is
        // precisely the state that made a production incident undiagnosable,
        // so a subscriber that captures every level would assert nothing.
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            // `with_ansi(false)`: the fmt layer is only here to populate the
            // span's `FormattedFields`, and styled output would interleave
            // escape codes between the field names being asserted.
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::io::sink),
            )
            .with(cap.clone());

        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 7,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        {
            let _g = tracing::subscriber::set_default(subscriber);
            s.submit(
                PathBuf::from("/m/x.mkv"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        }

        let events = cap.0.lock().unwrap().clone();
        let dispatch = events
            .iter()
            .find(|(_, fields)| fields.contains("transcode dispatch"))
            .unwrap_or_else(|| {
                panic!("no \"transcode dispatch\" event was recorded; events: {events:?}")
            });
        // The table's best device is the GPU, so this job decodes there — and
        // the record must SAY so rather than leaving it to be inferred from
        // the device name (a job on Nvenc:0 can still decode in software).
        assert!(
            dispatch.1.contains("decode_accel=gpu:cuda"),
            "dispatch record must name the decode path, got: {}",
            dispatch.1
        );
        assert!(
            dispatch.1.contains("decode_on_gpu=true"),
            "dispatch record must state whether decode was offloaded, got: {}",
            dispatch.1
        );
        assert!(
            dispatch.1.contains("device=Nvenc:0") && dispatch.1.contains("class=interactive"),
            "dispatch record must carry device + class, got: {}",
            dispatch.1
        );
        // Losing candidates: a CPU-only deployment and one whose GPU is in
        // cooldown look identical without them.
        assert!(
            dispatch.1.contains("candidates="),
            "dispatch record must name what the winner beat, got: {}",
            dispatch.1
        );
    }

    #[tokio::test]
    async fn non_recoverable_failure_returns_error() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
            WorkerRunResult::Failed(WorkerError::BadInput("scripted bad input".into()))
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await;
        assert_eq!(
            r,
            Err(SchedError::Failed(WorkerError::BadInput(
                "scripted bad input".into()
            )))
        );
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
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        assert_eq!(done.out_bytes, 9);
        // A second job still works → scheduler alive after a worker death.
        let done2 = s
            .submit(
                PathBuf::from("/m/y"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
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
            s.submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            ),
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
                    s2.submit(
                        PathBuf::from("/m/x"),
                        h264(),
                        file_sink(),
                        JobClass::Interactive,
                    )
                    .await
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
    async fn abandoned_queued_jobs_are_skipped_not_dispatched() {
        // Post-seek contention: hls.js aborts the in-flight prefetch fetches
        // for the OLD position when the user seeks; actix drops those handler
        // futures, dropping each `submit().await` and its oneshot receiver.
        // A queued job whose caller has gone must NOT burn a worker slot when
        // permits free — otherwise the seek-target segment waits behind dead
        // work and the user stares at a spinner. Proven by counting real
        // worker runs: only the blockers + the live queued job execute; the
        // abandoned jobs are dropped from the pending queue.
        let runs = Arc::new(AtomicU64::new(0));
        let r2 = runs.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(150), move |_, _| {
            r2.fetch_add(1, Ordering::SeqCst);
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());

        // 5 blockers occupy every permit (2 nvenc + 1 vaapi + 2 cpu).
        let mut blockers = Vec::new();
        for _ in 0..5 {
            let s2 = s.clone();
            blockers.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/block"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 10 "old position" jobs queue behind the blockers, then their
        // callers vanish (seek) — abort the tasks so the futures (and their
        // oneshot receivers) drop.
        let mut abandoned = Vec::new();
        for _ in 0..10 {
            let s2 = s.clone();
            abandoned.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/old"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        for h in &abandoned {
            h.abort();
        }
        // One legitimate seek-target job that stays queued behind the same
        // blockers — it MUST still complete (we skip only abandoned work).
        let s3 = s.clone();
        let seek_target = tokio::spawn(async move {
            s3.submit(
                PathBuf::from("/m/seek"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        for h in blockers {
            h.await.unwrap().unwrap();
        }
        let seek = tokio::time::timeout(Duration::from_secs(2), seek_target)
            .await
            .expect("seek-target hung behind abandoned work")
            .unwrap();
        assert!(seek.is_ok(), "seek-target segment must complete: {seek:?}");
        // Let any (erroneously) dispatched abandoned jobs finish so the count
        // is stable, then assert none ran.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            6,
            "only 5 blockers + 1 seek-target may run; abandoned queued jobs must be skipped"
        );
    }

    #[tokio::test]
    async fn snapshot_reports_capacity() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let snap = s.snapshot().await.unwrap();
        // 2 hw + cpu.
        assert_eq!(snap.devices.len(), 3);
        let total_cap: usize = snap.devices.iter().map(|d| d.capacity).sum();
        assert_eq!(total_cap, 2 + 1 + 2);
    }

    /// T020a — `pharos_transcode_pin_total{outcome}` is how V80's one-encoder
    /// guarantee is OBSERVED. A collision here is worse than a rename: fold
    /// `invalidated` into `followed` and a rendition whose device went away
    /// mid-stream reports as a healthy pin, so the alert for the #114 failure
    /// mode never fires while the dashboard looks fine.
    ///
    /// This is a real guard only because the strings live on `PinOutcome`.
    /// Asserting two inline literals at their call sites would have compared
    /// constants written in the test to constants written beside them —
    /// tautology that passes whatever the code does.
    #[test]
    fn pin_outcome_labels_are_distinct_and_stable() {
        const ALL: [PinOutcome; 3] = [
            PinOutcome::Followed,
            PinOutcome::Invalidated,
            PinOutcome::Unresolved,
        ];
        assert_eq!(PinOutcome::Followed.label(), "followed");
        assert_eq!(PinOutcome::Invalidated.label(), "invalidated");
        assert_eq!(PinOutcome::Unresolved.label(), "unresolved");

        let labels: std::collections::HashSet<&str> = ALL.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels.len(),
            ALL.len(),
            "pin outcome labels collide: {labels:?} — a folded bucket reports a \
             broken pin as a healthy one"
        );
    }

    #[test]
    fn job_class_labels_are_distinct_and_stable() {
        // These strings are a dashboard contract: they appear as the `class`
        // label on pharos_transcode_pending_by_class,
        // pharos_transcode_queue_wait_seconds and
        // pharos_segment_produced_total. A rename breaks every query silently.
        assert_eq!(JobClass::Interactive.label(), "interactive");
        assert_eq!(JobClass::Background.label(), "background");
        assert_ne!(JobClass::Interactive.label(), JobClass::Background.label());
    }

    #[tokio::test]
    async fn snapshot_attributes_the_queue_and_the_devices_to_who_is_waiting() {
        // The signal under test: a snapshot must say how much work is running
        // and queued for each class. Without the split, "5 running, 3 queued"
        // looks the same whether those five are stalled viewers or speculative
        // warm-up sitting in front of them.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(600), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // 5 permits total (2 nvenc + 1 vaapi + 2 cpu). Start with the pool
        // idle so speculative work is admitted, then fill the rest.
        let mut handles = Vec::new();
        for (class, n) in [(JobClass::Background, 2), (JobClass::Interactive, 3)] {
            for _ in 0..n {
                let s2 = s.clone();
                handles.push(tokio::spawn(async move {
                    s2.submit(PathBuf::from("/m/run"), h264(), file_sink(), class)
                        .await
                }));
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // Every permit is now held; further client requests queue.
        for _ in 0..3 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/queued"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(snap.inflight, 5, "every permit held by a segment job");
        assert_eq!(snap.pending, 3);
        assert_eq!(snap.pending_interactive, 3, "the queue is client requests");
        assert_eq!(snap.pending_background, 0);
        assert!(
            snap.oldest_pending_ms.is_some_and(|ms| ms > 0),
            "the head of the queue has measurably been waiting"
        );
        assert_eq!(snap.live_streams, 0, "no live stream took a permit");
        // Device occupancy is attributable to the jobs holding it: 3
        // interactive + 2 speculative, with nothing left unexplained.
        let by_class = |f: fn(&DeviceStat) -> usize| snap.devices.iter().map(f).sum::<usize>();
        assert_eq!(by_class(|d| d.inflight_interactive), 3);
        assert_eq!(by_class(|d| d.inflight_background), 2);
        assert_eq!(
            by_class(|d| d.inflight_interactive) + by_class(|d| d.inflight_background),
            by_class(|d| d.in_use),
            "no unexplained device occupancy"
        );

        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn speculative_work_is_shed_and_never_queues_in_front_of_a_client() {
        // The defect this pins: prefetch is dispatched BEFORE the segment the
        // client is blocked on, and shared one FIFO with it, so speculative
        // encodes could bury a client's own segment. Background work must now
        // run only out of spare capacity, and must never join the queue.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // 5 permits total (2 nvenc + 1 vaapi + 2 cpu). Occupy every one.
        let mut running = Vec::new();
        for _ in 0..5 {
            let s2 = s.clone();
            running.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/run"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Speculative work is refused immediately rather than queued.
        let shed = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
            )
            .await;
        assert_eq!(
            shed,
            Err(SchedError::Busy),
            "prefetch must be shed, not queued"
        );

        // A client request in the same state still queues and still completes.
        let s2 = s.clone();
        let client = tokio::spawn(async move {
            s2.submit(
                PathBuf::from("/m/client"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.pending_background, 0,
            "nothing speculative in the queue"
        );
        assert_eq!(snap.pending_interactive, 1, "the client request is queued");

        assert!(client.await.unwrap().is_ok());
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn speculative_work_runs_when_there_is_spare_capacity() {
        // Shedding must not become "never prefetch": with the pool idle, a
        // background job is admitted like any other.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 3,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
            )
            .await
            .expect("idle pool must accept speculative work");
        assert_eq!(done.out_bytes, 3);
    }

    #[tokio::test]
    async fn speculative_work_leaves_headroom_for_a_client() {
        // The last free permit is reserved: background is shed while only
        // `background_headroom` permits remain, so a client request arriving a
        // moment later still finds a slot instead of queueing behind a guess.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // Occupy 4 of the 5 permits, leaving exactly one free.
        let mut running = Vec::new();
        for _ in 0..4 {
            let s2 = s.clone();
            running.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/run"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            s.submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
            )
            .await,
            Err(SchedError::Busy),
            "the reserved permit is not for speculative work"
        );
        // ...and that reserved permit is there for the client.
        let done = s
            .submit(
                PathBuf::from("/m/client"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .expect("client must get the reserved permit");
        assert_eq!(done.queue_wait_ms, 0, "client did not wait");
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
    }
    /// A single GPU, so a job's peers are unambiguous.
    fn one_gpu(capacity: usize) -> DeviceTable {
        DeviceTable::from_probe(&[(DeviceId::hw(HwAccel::Nvenc, 0), capacity)], 0)
    }

    /// ODD — what a job encoded BESIDE is the thing that sets how long it took,
    /// and nothing recorded it.
    ///
    /// Measured on the deployment: one 6 s segment costs 1 860 ms alone and
    /// 6 229 ms when six speculative encodes share the device. `encode_ms`
    /// reports the 6 229 and cannot say why, so a slow encode is
    /// indistinguishable from a crowded one — the two need opposite fixes.
    #[tokio::test]
    async fn a_finished_job_reports_how_many_peers_shared_its_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());

        let first = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/a"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;

        let second = s
            .submit(
                PathBuf::from("/m/b"),
                h264(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .expect("second job must be admitted");
        assert_eq!(
            second.peer_jobs, 1,
            "a job dispatched onto a device already running one job has one peer"
        );
        assert!(first.await.unwrap().is_ok());
    }
}
